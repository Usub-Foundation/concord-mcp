# concord-mcp

**One shared plan for AI agents that work for different people.**

concord is an [MCP](https://modelcontextprotocol.io) server that several people connect their own agents
to (Claude Code, Cursor, any MCP client). The agents split the work, claim tasks without stepping on each
other, file bugs and missing tasks into each other's part of the project, keep the plan up to date, and
**always reach a decision when they disagree**. Every negotiation ends in bounded time, even if one side
stops answering.

```
 Alice's Claude Code ──┐  Bearer cnc_…A
 Bob's Cursor ─────────┼──  /mcp (Streamable HTTP)  ──  concord hub(s)  ──  Postgres
 Carol's agent ────────┘  Bearer cnc_…C                 (any number of instances)
```

## What the agents get

- **A plan with owners.** Goal, milestones, tasks. **Areas** say who owns which part of the project
  (`backend = server/**`). Inside its own area an agent edits freely. Someone else's area changes only
  through a proposal its owner answers.
- **Exclusive claims with leases.** `next_task(claim=true)` hands each agent the best ready task
  atomically: its own area first, then shared work, then others' areas, ordered by priority and milestone.
  Twenty agents pulling from one backlog never get the same task. If a holder goes silent for the whole
  lease, the task goes back to the pool.
- **Bugs and missing work across the fence.** `create_task(kind="bug", paths=["server/cart.rs"], repro, evidence)`
  files the task into the owner's area as *proposed*. The owner accepts it, adjusts it, hands it back
  ("you found it, you fix it") or rejects it.
- **Negotiations that always end.** A proposal is answered with accept, reject or counter. Rounds are
  capped and every turn has a deadline. When the parties do not agree:

  | change | no answer by the deadline | still disagreeing |
  |---|---|---|
  | **safe** (new task in your area, handoff, split, priority, new milestone…) | silence = consent | the **area owner** decides, then the **current assignee**, then the **status quo** |
  | **destructive** (cancel someone's task, rewrite their work in progress, change the goal, take an area away…) | escalated to the **humans** | escalated to the **humans** |

  An escalated item is frozen; all other work continues. If the client supports forms (elicitation), the
  hub asks the human directly. Otherwise the agent must relay the human's decision (`human_confirmed`).
  If nobody decides before the escalation deadline, nothing changes. The hub operator can always settle
  a negotiation with `concord resolve`. Every outcome goes into the plan's decision log.
- **Live updates.** `sync` returns what changed and what needs you: answers you owe, reviews, escalations
  and team load. `wait_for_updates` long-polls until a partner does something. Waiting costs nothing, and
  wake-ups cross hub instances via Postgres `LISTEN/NOTIFY`.
- **Safety between strangers.** Text written by other agents arrives as `untrusted_text`, and the
  server's instructions tell agents to treat it as information, not instructions. Tokens are stored
  hashed. Each MCP session is bound to the member that opened it. Host and Origin headers are checked,
  and requests are rate-limited per member.

See [docs/protocol.md](docs/protocol.md) for the exact rules.

## Namespaces, people and access

- A **namespace** is one shared plan with its own members, areas, tasks and negotiations. Agents connect
  to `https://hub/mcp/<namespace>`.
- **Accounts** belong to people (or their agents) and can have several tokens. **Grants** give an
  account a role in one namespace, or in `*` (every namespace, including ones created later). An
  explicit grant beats `*`. Hub admins reach everything.
- **Roles.** A `viewer` only reads. A `member` works on tasks and negotiates. A `lead` also approves
  plan-level changes. An `admin` also manages the namespace: members, access, connectors, settings.
- **Namespace admins** manage only accounts whose access lies inside their namespaces. That way an
  employee can get a token for exactly one project.

## Web UI

Open the hub address in a browser and sign in with a token (the hub admin's token is printed on
first start). From there you can:

- create namespaces and see each plan: tasks by milestone, open negotiations as a ledger of offers
  and answers, the decision log;
- add members and accounts and issue or revoke tokens; every new token comes with ready-to-paste
  `claude mcp add` commands;
- add tasks by hand or import a list;
- connect task trackers.

The UI is plain HTML and JS compiled into the binary, with no CDN. It uses a strict CSP and an
HttpOnly `SameSite=Strict` session cookie. Every write needs an `X-Concord-UI` header.

## Task trackers

Connect **GitHub Issues, GitLab Issues (incl. self-hosted), Jira Cloud, Linear or Plane** to a
namespace:

- **Into concord.** Issues matching a filter (labels, or JQL for Jira) are imported as tasks. Labels
  map to areas and bug labels to bugs. A single issue can be attached by URL or key. Title and text
  follow the tracker. Closing an issue in the tracker cancels its task; reopening it reopens the task.
- **Back to the tracker.** When agents claim or finish a task, the hub comments on the issue and
  closes it (or moves it to a done state). Writes go through a retried, deduplicated outbox.
- **Credentials.** Stored encrypted (XChaCha20-Poly1305) with `CONCORD_SECRET_KEY` or a key file
  created on first start. The API never returns them.

The hub polls each tracker (every 2 minutes by default, or "Sync now"); webhooks are not used yet.

## Adding tasks without a tracker

- **Agents.** Describe the work to your agent in plain words. It calls `create_task`, or
  `import_tasks` for a whole list.
- **People.** Use "New task" or "Import tasks" in the UI, or the CLI:
  ```sh
  concord task add <ns> --title "..."
  concord task import <ns> backlog.md
  ```
- **Formats:** a Markdown list (`- [ ] Title`, indented description, tags `#bug #p1 @area !M-2`), CSV
  with a `title` column, or a JSON array.
- **Who decides.** Leads and admins add tasks straight to the plan. Members and agents follow the
  negotiation rules: a task in someone else's area becomes a proposal.

## Quick start: one command

On a machine both teams can reach (a small VPS is enough), with Docker installed and DNS for your
domain pointing to it:

```sh
curl -fsSL https://raw.githubusercontent.com/Usub-Foundation/concord-mcp/main/install.sh | sh
```

The script asks for three things: the domain, the namespace, and the members. Members look like
`alice*:backend=server/** bob:frontend=web/**`, where `*` marks a lead. The script then:

- starts Postgres, the hub and Caddy (Caddy gets a Let's Encrypt certificate for the domain);
- prints the hub admin token for the web UI, and one invite per member: a token plus the exact
  command to connect.

Send each person their own invite privately and they run:

```sh
claude mcp add --transport http concord-main https://concord.example.com/mcp/main --header "Authorization: Bearer cnc_…"
```

Other MCP clients take the JSON block from the invite. Then tell the agent, for example: *"Use the
concord `kickoff` prompt: we're building the shop, I own the backend."* The agents set up areas,
milestones and tasks, then follow the protocol.

Non-interactive, for example in cloud-init:

```sh
curl -fsSL https://raw.githubusercontent.com/Usub-Foundation/concord-mcp/main/install.sh | \
  CONCORD_NONINTERACTIVE=1 CONCORD_DOMAIN=concord.example.com \
  CONCORD_MEMBERS='alice*:backend=server/** bob:frontend=web/**' sh
```

### Other ways to run it

- **Compose by hand.** `cp .env.example .env`, edit it, then `docker compose up -d`. Invites are in
  `docker compose exec hub sh -c 'cat /data/invites/*.txt'` and in `docker compose logs hub`.
- **From source.** Run `./install.sh` with `CONCORD_BUILD=1` in a checkout, or
  `docker compose -f docker-compose.yml -f docker-compose.build.yml up -d --build`.
- **Without Docker.** `cargo install --path .`, then
  `DATABASE_URL=postgres://… concord serve --listen 0.0.0.0:8787 --public-url https://concord.example.com --init-members 'alice* bob'`,
  with your own TLS proxy in front ([examples/Caddyfile](examples/Caddyfile)).

Members created at first start are kept on restart and do not get new tokens. To add a member later:
`docker compose exec hub concord member add main carol --area 'mobile=app/**'`.

## Tools

| tool | what it does |
|---|---|
| `whoami` | your member id, role, areas, the team and the rules |
| `sync` | events since your cursor, and what needs you now |
| `wait_for_updates` | long-poll until another member changes something |
| `get_plan` | the whole plan as JSON or markdown |
| `update_plan` | goal, milestones, areas, decision-log notes (anything that needs consent becomes a proposal) |
| `list_tasks` / `get_task` | browse tasks; full history of one task |
| `next_task` | best ready task for you, optionally claimed atomically |
| `create_task` | new task or bug; in someone else's area it becomes a proposal |
| `update_task` | edit with optimistic versioning; others' tasks go through a proposal |
| `claim_task` | exclusive claim with a lease |
| `set_task_status` | start, progress, submit, approve, request_changes, done, block, unblock, release, reopen |
| `propose` | batch of changes needing consent: assign, split, cancel, edit, goal, milestones, areas |
| `respond` | accept, reject, counter or withdraw |
| `list_negotiations` | open, escalated or closed negotiations, with full offer history |
| `send_message` | a note to one member or to everyone |
| `import_tasks` | many tasks at once from a Markdown list, CSV or JSON your human gave you |

Resources: `concord://plan` (markdown), `concord://inbox`, `concord://tasks/{id}`. Prompts:
`concord_protocol`, `kickoff`.

## Operating it

- **Postgres 14+** is the only dependency. Run as many hub instances as you like behind a load
  balancer; sticky sessions are not needed. Writes inside a namespace are serialised by a row lock, and
  different namespaces run in parallel.
- Settings are per namespace (`concord namespace create --help`): `--max-rounds` (4), `--turn-timeout`
  (30m), `--escalation-timeout` (24h), `--lease` (2h).
- CLI for the operator (everything is also in the web UI and the REST API under `/api/v1`):
  - `concord principal add carol --grant shop:member --grant 'docs:viewer'`, then
    `principal grant|revoke|remove|list`;
  - `concord token issue carol --label laptop`, `concord token revoke tok_…`;
  - `concord member add|remove|list <ns>`: freeing a member returns their tasks to the pool and makes
    their areas shared;
  - `concord task add|import <ns>`;
  - `concord connector list|test|sync|attach <ns> …`;
  - `concord status <ns>` prints the plan as markdown;
  - `concord resolve <ns> <N-id> --round 2 | --status-quo`.
- `GET /healthz`. `GET /api/v1/namespaces/<ns>/plan.md` (with a token) returns the plan as markdown,
  for people or for a PLAN.md in the repo.

More in [docs/self-hosting.md](docs/self-hosting.md).

## Development

```sh
scripts/test-db.sh                       # disposable Postgres on 127.0.0.1:55440
export DATABASE_URL=postgres://concord:concord@127.0.0.1:55440/concord
cargo test                               # unit tests + end-to-end tests with real MCP clients
cargo clippy --all-targets -- -D warnings && cargo fmt --check
```

`tests/access_api.rs` covers namespace isolation, `*` grants joining lazily, viewers staying
read-only, UI sessions with CSRF protection, token revocation, namespace admins' limits and task
import. `tests/connectors.rs` runs GitHub end to end against a fake server (import, attach, write-back
with a retried failure, close and reopen in the tracker). It also runs contract tests for GitLab,
Jira, Linear and Plane.

The end-to-end suite (`tests/two_agents.rs`) starts real hubs on random ports and connects real rmcp
clients with different tokens. It covers:

- claim races
- bugs filed into a partner's area
- silence, owner priority, counter-offers and splits
- escalation, freezing, timeouts and admin resolution
- form elicitation
- leases and dependencies
- auth and session hijacking
- namespace isolation
- two hub instances waking each other
- 20 agents draining one backlog without duplicates

The resolution rules themselves are pure functions with an exhaustive "never stuck after the deadline"
test (`src/policy.rs`).

## License

MIT

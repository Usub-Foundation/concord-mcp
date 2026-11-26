-- concord-mcp schema v1.
-- Every write to a namespace runs in a transaction that first locks the namespace row
-- (SELECT ... FOR UPDATE). That serialises writes inside one namespace, so per-namespace
-- event sequence numbers are assigned in commit order and never leave gaps for readers.

CREATE TABLE namespaces (
    id                      TEXT PRIMARY KEY,
    name                    TEXT NOT NULL,
    goal                    TEXT NOT NULL DEFAULT '',
    plan_version            BIGINT NOT NULL DEFAULT 1,
    event_seq               BIGINT NOT NULL DEFAULT 0,
    next_task_no            BIGINT NOT NULL DEFAULT 1,
    next_negotiation_no     BIGINT NOT NULL DEFAULT 1,
    next_milestone_no       BIGINT NOT NULL DEFAULT 1,
    max_rounds              INT    NOT NULL DEFAULT 4 CHECK (max_rounds >= 1),
    turn_timeout_secs       BIGINT NOT NULL DEFAULT 1800 CHECK (turn_timeout_secs > 0),
    escalation_timeout_secs BIGINT NOT NULL DEFAULT 86400 CHECK (escalation_timeout_secs > 0),
    lease_secs              BIGINT NOT NULL DEFAULT 7200 CHECK (lease_secs > 0),
    created_at              TIMESTAMPTZ NOT NULL
);

-- Accounts of people and their agents. A principal reaches namespaces through grants and
-- authenticates with one or more tokens.
CREATE TABLE principals (
    id           TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    human        TEXT NOT NULL DEFAULT '',
    -- Hub administrators manage every namespace, principal and token.
    is_admin     BOOLEAN NOT NULL DEFAULT FALSE,
    active       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL
);

-- Bearer tokens. Only the SHA-256 is stored; `id` is the public handle shown in lists.
CREATE TABLE tokens (
    id           TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    token_hash   TEXT NOT NULL UNIQUE,
    label        TEXT NOT NULL DEFAULT '',
    created_by   TEXT NOT NULL DEFAULT '',
    created_at   TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ
);
CREATE INDEX tokens_principal ON tokens (principal_id);

-- Access: one row per namespace, or namespace '*' for every namespace (including future ones).
CREATE TABLE grants (
    principal_id TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    namespace    TEXT NOT NULL,
    role         TEXT NOT NULL CHECK (role IN ('viewer', 'member', 'lead', 'admin')),
    created_at   TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (principal_id, namespace)
);
CREATE INDEX grants_namespace ON grants (namespace);

-- Membership inside a namespace (created on first access). `role` mirrors the grant.
CREATE TABLE members (
    namespace_id TEXT NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    id           TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    display_name TEXT NOT NULL,
    human        TEXT NOT NULL DEFAULT '',
    role         TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('viewer', 'member', 'lead', 'admin')),
    active       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL,
    last_seen_at TIMESTAMPTZ,
    PRIMARY KEY (namespace_id, id)
);

-- Web UI sessions (cookie). Only the SHA-256 of the session id is stored.
CREATE TABLE ui_sessions (
    id_hash      TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL REFERENCES principals (id) ON DELETE CASCADE,
    created_at   TIMESTAMPTZ NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL
);
CREATE INDEX ui_sessions_expires ON ui_sessions (expires_at);

CREATE TABLE areas (
    namespace_id TEXT NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    owner        TEXT,
    paths        JSONB NOT NULL DEFAULT '[]',
    description  TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (namespace_id, name)
);

CREATE TABLE milestones (
    namespace_id TEXT NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    id           TEXT NOT NULL,
    title        TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    position     BIGINT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'done')),
    PRIMARY KEY (namespace_id, id)
);

CREATE TABLE tasks (
    namespace_id   TEXT NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    id             TEXT NOT NULL,
    title          TEXT NOT NULL,
    description    TEXT NOT NULL DEFAULT '',
    acceptance     TEXT NOT NULL DEFAULT '',
    area           TEXT,
    milestone      TEXT,
    kind           TEXT NOT NULL,
    priority       TEXT NOT NULL,
    status         TEXT NOT NULL,
    assignee       TEXT,
    depends_on     JSONB NOT NULL DEFAULT '[]',
    created_by     TEXT NOT NULL,
    version        BIGINT NOT NULL DEFAULT 1,
    lease_until    TIMESTAMPTZ,
    blocked_reason TEXT,
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (namespace_id, id)
);
CREATE INDEX tasks_lease ON tasks (lease_until) WHERE lease_until IS NOT NULL;
CREATE INDEX tasks_assignee ON tasks (namespace_id, assignee) WHERE assignee IS NOT NULL;

CREATE TABLE negotiations (
    namespace_id  TEXT NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    id            TEXT NOT NULL,
    -- open | escalated | closed
    status        TEXT NOT NULL,
    -- agreed | silence | policy | status_quo | withdrawn | stale | humans | admin | escalation_timeout
    outcome       TEXT,
    applied_round INT,
    explanation   TEXT NOT NULL DEFAULT '',
    risk          TEXT NOT NULL,
    proposer      TEXT NOT NULL,
    participants  JSONB NOT NULL,
    targets       JSONB NOT NULL,
    owner         TEXT,
    incumbent     TEXT,
    round         INT NOT NULL,
    max_rounds    INT NOT NULL,
    stances       JSONB NOT NULL,
    deadline      TIMESTAMPTZ NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL,
    closed_at     TIMESTAMPTZ,
    PRIMARY KEY (namespace_id, id)
);
CREATE INDEX negotiations_due ON negotiations (deadline) WHERE status <> 'closed';

CREATE TABLE negotiation_offers (
    namespace_id   TEXT NOT NULL,
    negotiation_id TEXT NOT NULL,
    round          INT NOT NULL,
    author         TEXT NOT NULL,
    changes        JSONB NOT NULL,
    risk           TEXT NOT NULL,
    note           TEXT NOT NULL DEFAULT '',
    created_at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (namespace_id, negotiation_id, round),
    FOREIGN KEY (namespace_id, negotiation_id) REFERENCES negotiations (namespace_id, id) ON DELETE CASCADE
);

CREATE TABLE decisions (
    namespace_id   TEXT NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    seq            BIGINT NOT NULL,
    negotiation_id TEXT,
    outcome        TEXT NOT NULL,
    summary        TEXT NOT NULL,
    actor          TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (namespace_id, seq)
);

CREATE TABLE events (
    namespace_id TEXT NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    seq          BIGINT NOT NULL,
    at           TIMESTAMPTZ NOT NULL,
    actor        TEXT NOT NULL,
    kind         TEXT NOT NULL,
    entity       TEXT,
    -- NULL = visible to every member; otherwise a JSON array of member ids
    audience     JSONB,
    summary      TEXT NOT NULL,
    data         JSONB NOT NULL DEFAULT '{}',
    PRIMARY KEY (namespace_id, seq)
);
CREATE INDEX events_entity ON events (namespace_id, entity, seq);

-- MCP sessions shared by all hub instances (rmcp SessionStore) and bound to the member
-- that opened them, so a leaked session id cannot be reused with another token.
CREATE TABLE mcp_sessions (
    id           TEXT PRIMARY KEY,
    state        JSONB,
    namespace_id TEXT,
    member_id    TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX mcp_sessions_updated ON mcp_sessions (updated_at);

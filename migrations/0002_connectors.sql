-- Task-tracker connectors: import issues into a namespace, write status back.

CREATE TABLE connectors (
    id              TEXT PRIMARY KEY,
    namespace_id    TEXT NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    kind            TEXT NOT NULL CHECK (kind IN ('github', 'gitlab', 'jira', 'linear', 'plane')),
    name            TEXT NOT NULL,
    config          JSONB NOT NULL,
    -- XChaCha20-Poly1305: 24-byte nonce followed by the ciphertext of the tracker credential.
    secret_enc      BYTEA,
    secret_hint     TEXT NOT NULL DEFAULT '',
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    sync_every_secs BIGINT NOT NULL DEFAULT 120 CHECK (sync_every_secs >= 30),
    cursor          TIMESTAMPTZ,
    last_sync_at    TIMESTAMPTZ,
    last_error      TEXT,
    last_result     JSONB,
    created_by      TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL
);
CREATE INDEX connectors_namespace ON connectors (namespace_id);

-- A concord task that mirrors one tracker issue.
CREATE TABLE task_links (
    namespace_id        TEXT NOT NULL,
    task_id             TEXT NOT NULL,
    connector_id        TEXT NOT NULL REFERENCES connectors (id) ON DELETE CASCADE,
    external_id         TEXT NOT NULL,
    external_key        TEXT NOT NULL,
    url                 TEXT NOT NULL,
    external_open       BOOLEAN NOT NULL,
    external_state      TEXT NOT NULL DEFAULT '',
    external_updated_at TIMESTAMPTZ,
    linked_at           TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (namespace_id, task_id),
    UNIQUE (connector_id, external_id),
    FOREIGN KEY (namespace_id, task_id) REFERENCES tasks (namespace_id, id) ON DELETE CASCADE
);

-- Writes to trackers, recorded in the same transaction as the task change (transactional
-- outbox) and delivered with retries. `dedup_key` keeps a retried change from posting twice.
CREATE TABLE connector_outbox (
    id              BIGSERIAL PRIMARY KEY,
    connector_id    TEXT NOT NULL REFERENCES connectors (id) ON DELETE CASCADE,
    namespace_id    TEXT NOT NULL,
    task_id         TEXT NOT NULL,
    external_id     TEXT NOT NULL,
    external_key    TEXT NOT NULL,
    action          TEXT NOT NULL CHECK (action IN ('comment', 'complete')),
    body            TEXT NOT NULL DEFAULT '',
    dedup_key       TEXT NOT NULL UNIQUE,
    attempts        INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL,
    last_error      TEXT,
    done_at         TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL
);
CREATE INDEX connector_outbox_due ON connector_outbox (next_attempt_at) WHERE done_at IS NULL;

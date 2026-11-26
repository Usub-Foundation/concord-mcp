//! MCP session state in Postgres, so any hub instance can serve any session
//! (no sticky load balancing needed).

use rmcp::transport::streamable_http_server::session::{SessionState, SessionStore, SessionStoreError};
use sqlx::PgPool;
use sqlx::types::Json;

#[derive(Clone)]
pub struct PgSessionStore {
    pool: PgPool,
}

impl PgSessionStore {
    pub fn new(pool: PgPool) -> Self {
        PgSessionStore { pool }
    }

    /// Ties a session to the member that opened it. The first binding wins.
    pub async fn bind(&self, session: &str, namespace: &str, member: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO mcp_sessions (id, namespace_id, member_id) VALUES ($1, $2, $3)
             ON CONFLICT (id) DO UPDATE SET namespace_id = EXCLUDED.namespace_id, member_id = EXCLUDED.member_id
             WHERE mcp_sessions.member_id IS NULL",
        )
        .bind(session)
        .bind(namespace)
        .bind(member)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn owner(&self, session: &str) -> Result<Option<(String, String)>, sqlx::Error> {
        let row: Option<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT namespace_id, member_id FROM mcp_sessions WHERE id = $1")
                .bind(session)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(w, m)| Some((w?, m?))))
    }
}

#[async_trait::async_trait]
impl SessionStore for PgSessionStore {
    async fn load(&self, session_id: &str) -> Result<Option<SessionState>, SessionStoreError> {
        let row: Option<(Option<Json<SessionState>>,)> = sqlx::query_as("SELECT state FROM mcp_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|(s,)| s.map(|j| j.0)))
    }

    async fn store(&self, session_id: &str, state: &SessionState) -> Result<(), SessionStoreError> {
        sqlx::query(
            "INSERT INTO mcp_sessions (id, state) VALUES ($1, $2)
             ON CONFLICT (id) DO UPDATE SET state = EXCLUDED.state, updated_at = now()",
        )
        .bind(session_id)
        .bind(Json(state))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<(), SessionStoreError> {
        sqlx::query("DELETE FROM mcp_sessions WHERE id = $1").bind(session_id).execute(&self.pool).await?;
        Ok(())
    }
}

use serde_json::{Value, json};

/// Errors returned by hub operations.
///
/// Everything except `Db`/`Internal` is a *business* outcome the calling agent is expected
/// to read and react to (retry with a fresh version, open a proposal, ask its human...).
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("{0}")]
    NotFound(String),
    #[error("{message}")]
    Conflict { message: String, details: Value },
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Invalid(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T, E = HubError> = std::result::Result<T, E>;

impl HubError {
    pub fn conflict(message: impl Into<String>, details: Value) -> Self {
        HubError::Conflict { message: message.into(), details }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        HubError::Invalid(message.into())
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        HubError::NotFound(message.into())
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        HubError::Forbidden(message.into())
    }

    pub fn code(&self) -> &'static str {
        match self {
            HubError::NotFound(_) => "not_found",
            HubError::Conflict { .. } => "conflict",
            HubError::Forbidden(_) => "forbidden",
            HubError::Invalid(_) => "invalid",
            HubError::Unauthorized => "unauthorized",
            HubError::Db(_) | HubError::Internal(_) => "internal",
        }
    }

    /// JSON body handed back to the agent. Internal details are logged, not returned.
    pub fn to_json(&self) -> Value {
        match self {
            HubError::Db(e) => {
                tracing::error!(error = %e, "database error");
                json!({ "error": { "code": "internal", "message": "internal error, try again" } })
            }
            HubError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                json!({ "error": { "code": "internal", "message": "internal error, try again" } })
            }
            HubError::Conflict { message, details } => {
                json!({ "error": { "code": "conflict", "message": message, "details": details } })
            }
            other => json!({ "error": { "code": other.code(), "message": other.to_string() } }),
        }
    }
}

impl From<serde_json::Error> for HubError {
    fn from(e: serde_json::Error) -> Self {
        HubError::Internal(format!("json: {e}"))
    }
}

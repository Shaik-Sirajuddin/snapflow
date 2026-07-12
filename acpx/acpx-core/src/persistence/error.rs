//! Error type for the persistence module.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("json (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("background persistence task panicked or was cancelled: {0}")]
    TaskJoin(String),

    #[error("persistence connection mutex was poisoned by a prior panic")]
    Poisoned,

    #[error(
        "unknown transcript direction {0:?} (expected \"client_to_agent\" or \"agent_to_client\")"
    )]
    InvalidDirection(String),

    #[error("no session found for gateway_session_id {0:?}")]
    SessionNotFound(String),
}

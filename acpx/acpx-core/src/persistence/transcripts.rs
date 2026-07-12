//! Transcript persistence -- one JSON-RPC frame (in either direction)
//! exchanged on a gateway session, mirrored to the `transcripts` table for
//! replay/debugging. Phase 2 step 10.

use crate::persistence::PersistenceError;

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptRecord {
    /// Row id, `None` for a record not yet persisted (e.g. one being built
    /// by a caller ahead of [`crate::PersistenceStore::append_transcript`]).
    pub id: Option<i64>,
    pub gateway_session_id: String,
    pub direction: Direction,
    /// Kept as a parsed `serde_json::Value` rather than a raw `String` so
    /// callers on both the write and read path get structural access
    /// without an extra parse step; the store serializes to/from the
    /// `transcripts.payload` TEXT column internally.
    pub payload: serde_json::Value,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToAgent,
    AgentToClient,
}

impl Direction {
    /// Wire/DB representation, matching `schema.sql`'s column comment.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::ClientToAgent => "client_to_agent",
            Direction::AgentToClient => "agent_to_client",
        }
    }
}

impl TryFrom<&str> for Direction {
    type Error = PersistenceError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "client_to_agent" => Ok(Direction::ClientToAgent),
            "agent_to_client" => Ok(Direction::AgentToClient),
            other => Err(PersistenceError::InvalidDirection(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_round_trips_through_its_db_string() {
        for d in [Direction::ClientToAgent, Direction::AgentToClient] {
            assert_eq!(Direction::try_from(d.as_str()).unwrap(), d);
        }
    }

    #[test]
    fn unknown_direction_string_is_rejected() {
        assert!(Direction::try_from("sideways").is_err());
    }
}

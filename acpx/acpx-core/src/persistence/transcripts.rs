//! Transcript persistence. Phase 2 step 10 -- stub for now.

#[derive(Debug, Clone)]
pub struct TranscriptRecord {
    pub gateway_session_id: String,
    pub direction: Direction,
    pub payload: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    ClientToAgent,
    AgentToClient,
}

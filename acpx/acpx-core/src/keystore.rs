//! API key storage. Encryption-at-rest mechanism is an open risk (see
//! `05-open-risks.md`) -- stub until Phase 3 step 13 makes a decision.

#[derive(Debug, Clone)]
pub struct KeyRef(pub String);

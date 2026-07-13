//! Backend ACP agent process supervision ("sacp-conductor" in the task
//! draft). Phase 1 covers a single hardcoded process; Phase 2 generalizes to
//! N processes keyed by agent name with restart/backoff. See
//! `memory/acpx/gen/plans/acp-gateway-daemon/04-phased-plan.md`.

pub mod backoff;
pub mod framing;
pub mod process;
pub mod supervisor;
pub mod terminal;

pub use process::{BackendProcess, SpawnSpec};
pub use supervisor::{ProcessStatus, SharedBackendProcess, Supervisor, SupervisorError};
pub use terminal::{TerminalError, TerminalExitStatus, TerminalHandle};

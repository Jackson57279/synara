//! Local server supervision for the native client.
//!
//! Mirrors the Electron main process (`apps/desktop/src/main.ts` `startBackend`
//! and `backendSupervisionPolicy.ts`):
//!
//! - The server gets `SYNARA_PORT`, `SYNARA_HOME`, `SYNARA_AUTH_TOKEN`, and
//!   `SYNARA_NO_BROWSER=1`, plus `SYNARA_DESKTOP_PARENT_STDIN=1` with stdin held
//!   open, so the server shuts itself down when this process dies.
//! - Readiness is `GET /health` reporting `startupReady: true`. A successful
//!   spawn never resets the failure count; only readiness does.
//! - Failed starts back off 500 ms, doubling to 10 s, and the supervisor gives
//!   up after five in a row instead of respawning forever.
//! - A migration-recovery marker next to the database stops supervision: the
//!   recovery flow owns the database until it clears the marker.

mod health;
mod policy;
mod supervisor;
mod tail;

pub use health::{probe_health, HealthSnapshot};
pub use policy::{restart_delay, FailureResponse, RestartPolicy, SupervisionPolicy};
pub use supervisor::{
    migration_recovery_marker, ReadyServer, ServerCommand, ServerSpec, ServerStatus, Supervisor,
    WaitError,
};
pub use tail::{summarize_failure_output, OutputTail, OUTPUT_TAIL_CHARS};

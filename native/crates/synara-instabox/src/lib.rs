//! instabox sandboxes as Synara hosts.
//!
//! Each sandbox runs a full headless Synara server; the native client reaches
//! it through `instabox port-forward` like any other remote host (spec §8).
//! This crate drives the `instabox` CLI rather than its HTTP API so it uses
//! whatever account or `INSTABOX_API_KEY` the user already has, and never
//! handles provider credentials: those live in instabox Org Secrets and reach
//! the sandbox as placeholders.

mod cli;
mod host;
mod runner;
mod types;

pub use cli::{CreateOptions, ExecOptions, Instabox, InstaboxError};
pub use host::{HostError, PortForward, SynaraHost, SynaraHostSpec};
pub use runner::{CliRunner, RawOutput, Runner};
pub use types::{CliError, ExecResult, Sandbox, SandboxSpec, SecretMeta};

//! Wire-level types for the Synara WebSocket RPC.
//!
//! The server is `apps/server`; the source of truth for every shape here is
//! `packages/contracts`. Structs keep the fields the native client reads as
//! typed fields and carry everything else in `extra`, so a newer server can add
//! fields without breaking decoding and commands round-trip unknown fields.

pub mod commands;
pub mod events;
pub mod frames;
pub mod generated;
pub mod ids;
pub mod negotiate;
pub mod orchestration;
pub mod server;

pub use generated::{tags, RpcInfo};

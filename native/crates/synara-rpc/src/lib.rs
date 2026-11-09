//! WebSocket RPC client for `apps/server`.
//!
//! One actor task owns the socket. [`RpcClient`] is a cheap, cloneable handle
//! that sends commands to it. Streams are back-pressured the way Effect RPC
//! expects: the server sends the next `Chunk` only after the client `Ack`s the
//! previous one, and [`RpcStream`] acks a chunk only when the consumer pulls
//! it. Dropping an unfinished stream interrupts it on the server.

mod client;
mod error;
pub mod negotiate;

pub use client::{ClientConfig, RpcClient, RpcStream};
pub use error::{ConnectError, Disconnect, RpcError};

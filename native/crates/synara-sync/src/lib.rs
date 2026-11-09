//! Pure reducers for the two streams the client lives on.
//!
//! No I/O and no async: the session layer feeds stream items in, and the UI
//! reads the resulting state plus a change set describing what moved. Rules
//! mirror `apps/web/src/storeEventReducer.ts` and
//! `apps/web/src/messageTextSegments.ts`:
//!
//! - one global, non-contiguous `sequence`; an item at or below the cursor is
//!   already applied and is dropped;
//! - a snapshot overwrites the cursor with its `snapshotSequence`;
//! - removals leave tombstones that win over an older snapshot;
//! - a server-instance change drops every cursor (`reset`).

pub mod message;
pub mod shell;
pub mod thread;

pub use shell::{ShellChange, ShellStore, SidebarProject};
pub use thread::{ThreadChange, ThreadStatus, ThreadStore};

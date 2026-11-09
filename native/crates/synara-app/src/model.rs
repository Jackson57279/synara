//! What the window shows, derived from the session's stores. No GPUI types
//! here, so the derivation is tested without a window.

use std::collections::BTreeSet;
use std::ops::Range;

use synara_protocol::ids::{MessageId, ProjectId, ThreadId};
use synara_protocol::orchestration::{Message, MessageRole};
use synara_session::ConnectionState;
use synara_sync::{ShellStore, ThreadStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarRow {
    Project {
        id: ProjectId,
        title: String,
    },
    Thread {
        id: ThreadId,
        project_id: ProjectId,
        title: String,
        pinned: bool,
        /// A turn is running.
        working: bool,
        /// Waiting on an approval or a question.
        needs_attention: bool,
    },
}

/// Projects and their visible threads in display order (see
/// `ShellStore::sidebar`).
pub fn sidebar_rows(shell: &ShellStore) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    for entry in shell.sidebar(None) {
        rows.push(SidebarRow::Project {
            id: entry.project.id.clone(),
            title: entry.project.title.clone(),
        });
        for thread in entry.threads {
            let working = thread
                .latest_turn
                .as_ref()
                .is_some_and(|turn| turn.state == "running");
            rows.push(SidebarRow::Thread {
                id: thread.id.clone(),
                project_id: thread.project_id.clone(),
                title: if thread.title.trim().is_empty() {
                    "New thread".to_owned()
                } else {
                    thread.title.clone()
                },
                pinned: thread.is_pinned,
                working,
                needs_attention: thread.has_pending_approvals == Some(true)
                    || thread.has_pending_user_input == Some(true),
            });
        }
    }
    rows
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRow {
    pub id: MessageId,
    pub role: MessageRole,
    pub text: String,
    pub streaming: bool,
    /// Sent from here and not yet echoed by the server.
    pub pending: bool,
}

pub fn transcript_rows(store: &ThreadStore) -> Vec<TranscriptRow> {
    store
        .messages()
        .iter()
        .map(|message: &Message| TranscriptRow {
            id: message.id.clone(),
            role: message.role,
            text: message.text.clone(),
            streaming: message.streaming,
            pending: store.is_pending(&message.id),
        })
        .collect()
}

/// How to bring a `ListState` from `old` rows to `new` rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListUpdate {
    /// Nothing moved.
    None,
    /// Re-create every row (snapshot, history rewrite, trimmed head).
    Reset(usize),
    /// `(old range, new count)` splices, applied in order. Rows outside them
    /// keep their measured heights, which keeps end-follow stable.
    Splice(Vec<(Range<usize>, usize)>),
}

/// Plans the smallest list update. Rows whose id stayed in place but whose
/// content changed (`changed`) are re-spliced so they are re-measured.
pub fn plan_list_update(
    old: &[MessageId],
    new: &[MessageId],
    changed: &BTreeSet<MessageId>,
    reset: bool,
) -> ListUpdate {
    if reset {
        return ListUpdate::Reset(new.len());
    }
    let shared = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    // Anything other than "same prefix, then appended" (an insert, a removal,
    // a trimmed head) is rare enough to rebuild.
    if shared < old.len() {
        return ListUpdate::Reset(new.len());
    }
    let mut splices: Vec<(Range<usize>, usize)> = new[..shared]
        .iter()
        .enumerate()
        .filter(|(_, id)| changed.contains(*id))
        .map(|(ix, _)| (ix..ix + 1, 1))
        .collect();
    if new.len() > shared {
        splices.push((shared..shared, new.len() - shared));
    }
    if splices.is_empty() {
        ListUpdate::None
    } else {
        ListUpdate::Splice(splices)
    }
}

/// One line for the status bar, and whether it is a problem.
pub fn connection_label(state: &ConnectionState) -> (String, bool) {
    match state {
        ConnectionState::Idle => ("Not connected".into(), false),
        ConnectionState::Connecting { attempt } if *attempt <= 1 => ("Connecting…".into(), false),
        ConnectionState::Connecting { attempt } => {
            (format!("Reconnecting (attempt {attempt})…"), true)
        }
        ConnectionState::Connected { server_build, .. } => {
            (format!("Connected · server {server_build}"), false)
        }
        ConnectionState::Waiting { reason, retry_in } => (
            format!(
                "Connection lost: {reason}. Retrying in {}s",
                retry_in.as_secs().max(1)
            ),
            true,
        ),
        ConnectionState::Incompatible(error) => (format!("Incompatible server: {error}"), true),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn ids(names: &[&str]) -> Vec<MessageId> {
        names.iter().map(|n| MessageId::from(*n)).collect()
    }

    #[test]
    fn appends_splice_only_the_tail() {
        let update = plan_list_update(
            &ids(&["a", "b"]),
            &ids(&["a", "b", "c", "d"]),
            &BTreeSet::new(),
            false,
        );
        assert_eq!(update, ListUpdate::Splice(vec![(2..2, 2)]));
    }

    #[test]
    fn streaming_text_re_splices_the_changed_row() {
        let changed = BTreeSet::from([MessageId::from("b")]);
        let update = plan_list_update(&ids(&["a", "b"]), &ids(&["a", "b"]), &changed, false);
        assert_eq!(update, ListUpdate::Splice(vec![(1..2, 1)]));
        let update = plan_list_update(&ids(&["a"]), &ids(&["a"]), &BTreeSet::new(), false);
        assert_eq!(update, ListUpdate::None);
    }

    #[test]
    fn rewrites_and_snapshots_reset() {
        let none = BTreeSet::new();
        assert_eq!(
            plan_list_update(&ids(&["a", "b"]), &ids(&["b", "c"]), &none, false),
            ListUpdate::Reset(2)
        );
        assert_eq!(
            plan_list_update(&ids(&["a", "b"]), &ids(&["a"]), &none, false),
            ListUpdate::Reset(1)
        );
        assert_eq!(
            plan_list_update(&ids(&["a"]), &ids(&["a", "b"]), &none, true),
            ListUpdate::Reset(2)
        );
    }

    #[test]
    fn labels_connection_states() {
        assert_eq!(
            connection_label(&ConnectionState::Connecting { attempt: 1 }),
            ("Connecting…".into(), false)
        );
        let (text, problem) = connection_label(&ConnectionState::Waiting {
            reason: "socket closed".into(),
            retry_in: Duration::from_millis(300),
        });
        assert!(problem);
        assert_eq!(text, "Connection lost: socket closed. Retrying in 1s");
    }
}

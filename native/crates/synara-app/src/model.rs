//! What the window shows, derived from the session's stores. No GPUI types
//! here, so the derivation is tested without a window.

use std::collections::BTreeSet;
use std::ops::Range;

use synara_protocol::ids::{MessageId, ProjectId, ThreadId};
use synara_protocol::orchestration::{Message, MessageRole, Thread};
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

/// The thread's error banner text: `session.lastError`, except the one
/// message the web client treats as non-fatal
/// (`normalizeThreadErrorMessage` in `apps/web/src/storeNormalization.ts`).
pub fn thread_error(thread: &Thread) -> Option<String> {
    let error = thread.session.as_ref()?.last_error.as_deref()?.trim();
    let non_fatal = error
        .to_lowercase()
        .contains("write_stdin failed: stdin is closed for this session");
    (!error.is_empty() && !non_fatal).then(|| error.to_owned())
}

/// Longest failure reason shown, as in `MAX_TURN_FAILURE_REASON_CHARS`.
const MAX_FAILURE_REASON_CHARS: usize = 80;

/// The error banner: the session's `lastError` when there is one, otherwise
/// why the latest turn failed. Providers often record a failed turn only as
/// `runtime.error` / failed `turn.completed` activities and leave
/// `lastError` empty (a Codex 401 does this).
pub fn thread_banner(thread: &Thread) -> Option<String> {
    thread_error(thread).or_else(|| {
        latest_turn_failure(thread).map(|reason| match reason {
            Some(reason) => format!("Turn failed: {reason}"),
            None => "Turn failed".to_owned(),
        })
    })
}

/// `Some(reason)` when the latest turn failed, mirroring
/// `deriveTurnTimingByTurnId` in
/// `apps/web/src/components/chat/MessagesTimeline.logic.ts` for that turn: a
/// failure activity marks it failed unless the turn later completed, and
/// `latestTurn.state == "error"` marks it failed without a reason.
pub fn latest_turn_failure(thread: &Thread) -> Option<Option<String>> {
    let turn = thread.latest_turn.as_ref()?;
    let mut failed = false;
    let mut reason = None;
    let mut settled = None;
    for activity in &thread.activities {
        if activity.turn_id.as_ref() != Some(&turn.turn_id) {
            continue;
        }
        let state = activity.payload.get("state").and_then(|v| v.as_str());
        match activity.kind.as_str() {
            "turn.completed" => settled = state.map(str::to_owned),
            "turn.aborted" => settled = Some("interrupted".to_owned()),
            _ => {}
        }
        let stopped = activity.kind == "turn.aborted"
            || (activity.kind == "turn.completed"
                && matches!(state, Some("interrupted" | "cancelled")));
        if stopped {
            continue;
        }
        let is_failure = activity.kind == "runtime.error"
            || (activity.kind == "turn.completed"
                && (state == Some("failed") || activity.tone == "error"));
        if is_failure {
            failed = true;
            let text = |key: &str| {
                activity
                    .payload
                    .get(key)
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
            };
            let message = text("errorMessage").or_else(|| text("message"));
            reason = summarize_failure_reason(text("errorCode"), message).or(reason);
        }
    }
    if failed && settled.as_deref() == Some("completed") {
        failed = false;
        reason = None;
    }
    if !failed && turn.state == "error" {
        failed = true;
    }
    failed.then_some(reason)
}

/// `summarizeTurnFailureReason`: short names for capacity and quota
/// failures, otherwise the message's first line, capped.
fn summarize_failure_reason(error_code: Option<&str>, message: Option<&str>) -> Option<String> {
    let message = message.unwrap_or_default().trim();
    let lower = message.to_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if error_code == Some("server_overloaded") || any(&["at capacity", "overloaded"]) {
        return Some("model at capacity".into());
    }
    if any(&["usage limit", "rate limit", "quota", "out of credits"]) {
        return Some("usage limit reached".into());
    }
    let first = message.lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        return None;
    }
    if first.chars().count() > MAX_FAILURE_REASON_CHARS {
        let cut: String = first.chars().take(MAX_FAILURE_REASON_CHARS - 1).collect();
        return Some(format!("{}…", cut.trim_end()));
    }
    Some(first.to_owned())
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
    fn thread_error_skips_the_non_fatal_stdin_message() {
        let mut thread: Thread = serde_json::from_value(serde_json::json!({
            "id": "t", "projectId": "p", "title": "", "messages": [],
            "proposedPlans": [], "activities": [], "latestTurn": null,
            "session": {"threadId": "t", "status": "error", "providerName": "codex",
                        "runtimeMode": "full-access", "activeTurnId": null,
                        "lastError": "  Not signed in to Codex  ", "updatedAt": "x"},
            "createdAt": "x", "updatedAt": "x"
        }))
        .unwrap();
        assert_eq!(
            thread_error(&thread).as_deref(),
            Some("Not signed in to Codex")
        );
        thread.session.as_mut().unwrap().last_error =
            Some("write_stdin failed: stdin is closed for this session".into());
        assert_eq!(thread_error(&thread), None);
        thread.session = None;
        assert_eq!(thread_error(&thread), None);
    }

    fn thread_with(activities: serde_json::Value, latest_state: &str) -> Thread {
        serde_json::from_value(serde_json::json!({
            "id": "t", "projectId": "p", "title": "", "messages": [],
            "proposedPlans": [], "activities": activities,
            "latestTurn": {"turnId": "turn-1", "state": latest_state,
                           "requestedAt": "x", "startedAt": null, "completedAt": null,
                           "assistantMessageId": null},
            "session": null, "createdAt": "x", "updatedAt": "x"
        }))
        .unwrap()
    }

    fn activity(kind: &str, tone: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"id": kind, "tone": tone, "kind": kind, "summary": "",
                           "payload": payload, "turnId": "turn-1", "createdAt": "x"})
    }

    #[test]
    fn latest_turn_failure_reads_the_failure_activities() {
        let unauthorized = "unexpected status 401 Unauthorized: Missing bearer or basic \
            authentication in header, url: https://api.openai.com/v1/responses";
        let thread = thread_with(
            serde_json::json!([
                activity(
                    "runtime.error",
                    "error",
                    serde_json::json!({"message": unauthorized})
                ),
                activity(
                    "turn.completed",
                    "error",
                    serde_json::json!({"state": "failed", "errorMessage": unauthorized})
                ),
            ]),
            "error",
        );
        let banner = thread_banner(&thread).unwrap();
        assert!(banner.starts_with("Turn failed: unexpected status 401"));
        assert!(banner.ends_with('…'));
        assert_eq!(banner.chars().count(), "Turn failed: ".len() + 80);
    }

    #[test]
    fn latest_turn_failure_names_quota_and_ignores_recovered_turns() {
        let quota = thread_with(
            serde_json::json!([activity(
                "runtime.error",
                "error",
                serde_json::json!({"message": "You hit your usage limit"})
            )]),
            "running",
        );
        assert_eq!(
            latest_turn_failure(&quota),
            Some(Some("usage limit reached".into()))
        );
        let recovered = thread_with(
            serde_json::json!([
                activity(
                    "runtime.error",
                    "error",
                    serde_json::json!({"message": "blip"})
                ),
                activity(
                    "turn.completed",
                    "info",
                    serde_json::json!({"state": "completed"})
                ),
            ]),
            "completed",
        );
        assert_eq!(thread_banner(&recovered), None);
        assert_eq!(
            latest_turn_failure(&thread_with(serde_json::json!([]), "error")),
            Some(None)
        );
        let stopped = thread_with(
            serde_json::json!([activity("turn.aborted", "info", serde_json::json!({}))]),
            "interrupted",
        );
        assert_eq!(thread_banner(&stopped), None);
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

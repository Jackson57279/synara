//! Per-thread reducer for `orchestration.subscribeThread`.

use std::collections::BTreeSet;

use synara_protocol::events::{MessageSentPayload, OrchestrationEvent, ThreadEvent};
use synara_protocol::ids::{MessageId, ThreadId};
use synara_protocol::orchestration::{
    Extra, LatestTurn, Message, MessageRole, Thread, ThreadDetailSnapshot, ThreadStreamItem,
};

use crate::message::{merge_message, message_from_payload};

/// Mirrors `MAX_THREAD_MESSAGES` in `apps/web/src/storeNormalization.ts`.
pub const MAX_THREAD_MESSAGES: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatus {
    /// Subscribed; no snapshot yet.
    Loading,
    Ready,
    /// History was rewritten (revert/rollback) or an event could not be applied:
    /// the session must resubscribe without a cursor.
    NeedsSnapshot,
    Deleted,
}

/// What one `apply` changed, so the UI only touches what moved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadChange {
    /// Replaced wholesale (snapshot): rebuild every row.
    pub reset: bool,
    /// Messages whose content changed, including new ones.
    pub messages: BTreeSet<MessageId>,
    /// New messages were appended at the tail.
    pub appended: bool,
    pub activities: bool,
    pub plans: bool,
    pub session: bool,
    pub meta: bool,
    /// `pendingInteractions` changed.
    pub pending: bool,
}

impl ThreadChange {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn absorb(&mut self, other: Self) {
        self.reset |= other.reset;
        self.messages.extend(other.messages);
        self.appended |= other.appended;
        self.activities |= other.activities;
        self.plans |= other.plans;
        self.session |= other.session;
        self.meta |= other.meta;
        self.pending |= other.pending;
    }
}

#[derive(Debug, Clone)]
pub struct ThreadStore {
    pub thread_id: ThreadId,
    cursor: Option<u64>,
    status: ThreadStatus,
    thread: Option<Thread>,
    /// Locally sent user messages not yet echoed by the server.
    pending: BTreeSet<MessageId>,
    /// Events that failed to decode; kept for logs, never rendered.
    pub decode_errors: u64,
}

impl ThreadStore {
    pub fn new(thread_id: ThreadId) -> Self {
        Self {
            thread_id,
            cursor: None,
            status: ThreadStatus::Loading,
            thread: None,
            pending: BTreeSet::new(),
            decode_errors: 0,
        }
    }

    pub fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    pub fn status(&self) -> ThreadStatus {
        self.status
    }

    pub fn thread(&self) -> Option<&Thread> {
        self.thread.as_ref()
    }

    pub fn messages(&self) -> &[Message] {
        self.thread.as_ref().map_or(&[], |t| t.messages.as_slice())
    }

    pub fn is_pending(&self, message_id: &MessageId) -> bool {
        self.pending.contains(message_id)
    }

    /// Whether the transcript should show the assistant as working.
    pub fn is_working(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| {
            t.session.as_ref().is_some_and(|s| s.is_running())
                || t.latest_turn.as_ref().is_some_and(LatestTurn::is_running)
        })
    }

    /// Forget the cursor (server instance changed); the next subscribe snapshots.
    pub fn reset_cursor(&mut self) {
        self.cursor = None;
        if self.status == ThreadStatus::Ready {
            self.status = ThreadStatus::Loading;
        }
    }

    /// The `afterSequence` to resubscribe with, if a resume is safe.
    pub fn resume_cursor(&self) -> Option<u64> {
        match self.status {
            ThreadStatus::Ready => self.cursor,
            _ => None,
        }
    }

    pub fn apply(&mut self, item: ThreadStreamItem) -> ThreadChange {
        match item {
            ThreadStreamItem::Snapshot { snapshot } => self.apply_snapshot(*snapshot),
            ThreadStreamItem::Event { event } => self.apply_event(&event),
            ThreadStreamItem::Replay { events } => {
                let mut change = ThreadChange::default();
                for event in &events {
                    change.absorb(self.apply_event(event));
                }
                change
            }
            ThreadStreamItem::Unknown => ThreadChange::default(),
        }
    }

    fn apply_snapshot(&mut self, snapshot: ThreadDetailSnapshot) -> ThreadChange {
        let mut thread = snapshot.thread;
        // A pending message the snapshot already has was echoed; the rest stay
        // visible until their echo or rejection arrives.
        let previous = self.thread.take();
        self.pending
            .retain(|id| !thread.messages.iter().any(|m| &m.id == id));
        if let Some(previous) = previous {
            thread.messages.extend(
                previous
                    .messages
                    .into_iter()
                    .filter(|m| self.pending.contains(&m.id)),
            );
        }
        self.status = if thread.deleted_at.is_some() {
            ThreadStatus::Deleted
        } else {
            ThreadStatus::Ready
        };
        self.cursor = Some(snapshot.snapshot_sequence);
        self.thread = Some(thread);
        ThreadChange {
            reset: true,
            ..ThreadChange::default()
        }
    }

    fn apply_event(&mut self, event: &OrchestrationEvent) -> ThreadChange {
        if self.cursor.is_some_and(|cursor| event.sequence <= cursor) {
            return ThreadChange::default();
        }
        if self.thread.is_none() {
            // Events before the first snapshot cannot be applied; the server
            // always sends the snapshot first, so this only guards bugs.
            return ThreadChange::default();
        }
        self.cursor = Some(event.sequence);
        let decoded = match event.decode() {
            Ok(decoded) => decoded,
            Err(_) => {
                self.decode_errors += 1;
                return ThreadChange::default();
            }
        };
        let Some(thread) = self.thread.as_mut() else {
            return ThreadChange::default();
        };
        let mut change = ThreadChange::default();
        match decoded {
            ThreadEvent::MessageSent(payload) => {
                if payload.thread_id != thread.id {
                    return change;
                }
                apply_message_sent(thread, &payload, event.sequence, &mut change);
                if self.pending.remove(&payload.message_id) {
                    if let Some(message) = thread
                        .messages
                        .iter_mut()
                        .find(|m| m.id == payload.message_id)
                    {
                        message.extra.remove(LOCAL_MARK);
                    }
                    change.messages.insert(payload.message_id.clone());
                }
            }
            ThreadEvent::ActivityAppended(payload) => {
                let activity = payload.activity;
                if crate::pending::reconcile_activity(
                    &thread.id,
                    &mut thread.pending_interactions,
                    &activity,
                ) {
                    change.pending = true;
                }
                match thread.activities.iter_mut().find(|a| a.id == activity.id) {
                    Some(existing) => *existing = activity,
                    None => thread.activities.push(activity),
                }
                change.activities = true;
            }
            ThreadEvent::SessionSet(payload) => {
                thread.session = Some(payload.session);
                change.session = true;
            }
            ThreadEvent::ProposedPlanUpserted(payload) => {
                let plan = payload.proposed_plan;
                match thread.proposed_plans.iter_mut().find(|p| p.id == plan.id) {
                    Some(existing) => *existing = plan,
                    None => thread.proposed_plans.push(plan),
                }
                change.plans = true;
            }
            ThreadEvent::MetaUpdated(payload) => {
                if let Some(title) = payload.title {
                    thread.title = title;
                }
                change.meta = true;
            }
            ThreadEvent::Deleted(payload) => {
                thread.deleted_at = Some(payload.deleted_at);
                self.status = ThreadStatus::Deleted;
                change.meta = true;
            }
            ThreadEvent::ResponseRequested {
                approval,
                command_id,
                payload,
            } => {
                if payload.thread_id == thread.id
                    && crate::pending::mark_responding(
                        &mut thread.pending_interactions,
                        approval,
                        command_id.as_ref(),
                        &payload,
                    )
                {
                    change.pending = true;
                }
            }
            ThreadEvent::HistoryRewritten { .. } => {
                self.status = ThreadStatus::NeedsSnapshot;
                change.meta = true;
            }
            ThreadEvent::Other => {}
        }
        change
    }

    /// Shows a user message immediately while its `thread.turn.start` is in flight.
    pub fn add_pending_user_message(
        &mut self,
        message_id: MessageId,
        text: String,
        created_at: String,
    ) -> ThreadChange {
        let Some(thread) = self.thread.as_mut() else {
            return ThreadChange::default();
        };
        if thread.messages.iter().any(|m| m.id == message_id) {
            return ThreadChange::default();
        }
        let mut extra = Extra::new();
        extra.insert(LOCAL_MARK.to_owned(), serde_json::Value::Bool(true));
        thread.messages.push(Message {
            id: message_id.clone(),
            role: MessageRole::User,
            text,
            text_segments: None,
            turn_id: None,
            streaming: false,
            created_at: created_at.clone(),
            updated_at: created_at,
            extra,
        });
        self.pending.insert(message_id.clone());
        ThreadChange {
            messages: BTreeSet::from([message_id]),
            appended: true,
            ..ThreadChange::default()
        }
    }

    /// Drops an optimistic message whose dispatch was rejected.
    pub fn remove_pending_user_message(&mut self, message_id: &MessageId) -> ThreadChange {
        if !self.pending.remove(message_id) {
            return ThreadChange::default();
        }
        if let Some(thread) = self.thread.as_mut() {
            thread.messages.retain(|m| &m.id != message_id);
        }
        ThreadChange {
            reset: true,
            ..ThreadChange::default()
        }
    }
}

/// Marks optimistic messages so a snapshot can tell them from server rows.
/// Never serialized to the server: the transcript only reads it.
const LOCAL_MARK: &str = "__nativePending";

fn apply_message_sent(
    thread: &mut Thread,
    payload: &MessageSentPayload,
    sequence: u64,
    change: &mut ThreadChange,
) {
    // Streaming deltas target the newest message; scan from the tail.
    let existing_index = thread
        .messages
        .iter()
        .rposition(|m| m.id == payload.message_id);
    match existing_index {
        Some(index) => {
            if let Some(merged) = merge_message(&thread.messages[index], payload, sequence) {
                thread.messages[index] = merged;
                change.messages.insert(payload.message_id.clone());
            }
        }
        None => {
            thread.messages.push(message_from_payload(payload));
            if thread.messages.len() > MAX_THREAD_MESSAGES {
                let overflow = thread.messages.len() - MAX_THREAD_MESSAGES;
                thread.messages.drain(..overflow);
                change.reset = true;
            }
            change.messages.insert(payload.message_id.clone());
            change.appended = true;
        }
    }

    if payload.role == MessageRole::Assistant {
        if let Some(turn_id) = &payload.turn_id {
            let previous = thread.latest_turn.as_ref();
            if previous.is_none_or(|turn| &turn.turn_id == turn_id) {
                // A settled message does not end its turn while the session still runs it.
                let still_running = payload.streaming
                    || thread.session.as_ref().is_some_and(|s| {
                        s.is_running() && s.active_turn_id.as_ref() == Some(turn_id)
                    });
                let state = if still_running {
                    "running"
                } else {
                    match previous.map(|t| t.state.as_str()) {
                        Some("interrupted") => "interrupted",
                        Some("error") => "error",
                        _ => "completed",
                    }
                };
                let next = LatestTurn {
                    turn_id: turn_id.clone(),
                    state: state.to_owned(),
                    requested_at: previous
                        .map_or_else(|| payload.created_at.clone(), |t| t.requested_at.clone()),
                    started_at: previous
                        .and_then(|t| t.started_at.clone())
                        .or_else(|| Some(payload.created_at.clone())),
                    completed_at: if still_running {
                        previous.and_then(|t| t.completed_at.clone())
                    } else {
                        Some(payload.updated_at.clone())
                    },
                    assistant_message_id: Some(payload.message_id.clone()),
                    extra: previous.map(|t| t.extra.clone()).unwrap_or_default(),
                };
                if thread.latest_turn.as_ref() != Some(&next) {
                    thread.latest_turn = Some(next);
                    change.session = true;
                }
            }
        }
    }
    if payload.updated_at > thread.updated_at {
        thread.updated_at = payload.updated_at.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::{json, Value};

    fn snapshot(sequence: u64, messages: Value) -> ThreadStreamItem {
        serde_json::from_value(json!({
            "kind": "snapshot",
            "snapshot": {
                "snapshotSequence": sequence,
                "thread": {
                    "id": "t-1", "projectId": "p-1", "title": "T", "deletedAt": null,
                    "messages": messages, "activities": [], "latestTurn": null, "session": null,
                    "createdAt": "2026-11-09T00:00:00.000Z", "updatedAt": "2026-11-09T00:00:00.000Z",
                    "checkpoints": []
                }
            }
        }))
        .unwrap()
    }

    fn event(sequence: u64, event_type: &str, payload: Value) -> ThreadStreamItem {
        serde_json::from_value(json!({
            "kind": "event",
            "event": {
                "sequence": sequence, "eventId": format!("e{sequence}"), "aggregateKind": "thread",
                "aggregateId": "t-1", "occurredAt": "x", "commandId": null, "type": event_type,
                "payload": payload
            }
        }))
        .unwrap()
    }

    fn sent(sequence: u64, id: &str, role: &str, text: &str, streaming: bool) -> ThreadStreamItem {
        event(
            sequence,
            "thread.message-sent",
            json!({
                "threadId": "t-1", "messageId": id, "role": role, "text": text, "turnId": "turn-1",
                "streaming": streaming, "createdAt": "2026-11-09T00:00:01.000Z",
                "updatedAt": format!("2026-11-09T00:00:{:02}.000Z", sequence % 60)
            }),
        )
    }

    fn ready_store() -> ThreadStore {
        let mut store = ThreadStore::new("t-1".into());
        assert!(store.apply(snapshot(10, json!([]))).reset);
        store
    }

    #[test]
    fn snapshot_sets_cursor_and_status() {
        let store = ready_store();
        assert_eq!(store.cursor(), Some(10));
        assert_eq!(store.status(), ThreadStatus::Ready);
        assert_eq!(store.resume_cursor(), Some(10));
    }

    #[test]
    fn drops_events_at_or_below_cursor() {
        let mut store = ready_store();
        assert!(store
            .apply(sent(10, "m", "assistant", "old", true))
            .is_empty());
        assert!(store.messages().is_empty());
        let change = store.apply(sent(11, "m", "assistant", "new", true));
        assert!(change.appended);
        assert_eq!(store.cursor(), Some(11));
    }

    #[test]
    fn streams_then_settles_latest_turn() {
        let mut store = ready_store();
        store.apply(sent(11, "m", "assistant", "Hel", true));
        store.apply(sent(12, "m", "assistant", "lo", true));
        assert!(store.is_working());
        assert_eq!(store.messages()[0].text, "Hello");
        let change = store.apply(sent(13, "m", "assistant", "Hello", false));
        assert!(change.messages.contains(&MessageId::from("m")));
        assert!(!change.appended);
        assert!(!store.is_working());
        let turn = store.thread().unwrap().latest_turn.as_ref().unwrap();
        assert_eq!(turn.state, "completed");
        assert_eq!(turn.assistant_message_id, Some("m".into()));
    }

    #[test]
    fn replay_applies_as_one_change() {
        let mut store = ready_store();
        let replay: ThreadStreamItem = serde_json::from_value(json!({
            "kind": "replay",
            "events": [
                serde_json::to_value(match sent(11, "a", "assistant", "x", true) { ThreadStreamItem::Event { event } => *event, _ => unreachable!() }).unwrap(),
                serde_json::to_value(match sent(12, "b", "assistant", "y", false) { ThreadStreamItem::Event { event } => *event, _ => unreachable!() }).unwrap(),
            ]
        }))
        .unwrap();
        let change = store.apply(replay);
        assert_eq!(change.messages.len(), 2);
        assert_eq!(store.cursor(), Some(12));
    }

    #[test]
    fn pending_user_message_reconciles_with_echo() {
        let mut store = ready_store();
        store.add_pending_user_message("u".into(), "hi".into(), "2026-11-09T00:00:00.500Z".into());
        assert!(store.is_pending(&"u".into()));
        store.apply(sent(11, "u", "user", "hi", false));
        assert!(!store.is_pending(&"u".into()));
        assert_eq!(store.messages().len(), 1);
        assert!(!store.messages()[0].extra.contains_key(LOCAL_MARK));
    }

    #[test]
    fn pending_message_survives_a_snapshot_and_can_be_removed() {
        let mut store = ready_store();
        store.add_pending_user_message("u".into(), "hi".into(), "x".into());
        store.apply(snapshot(20, json!([])));
        assert_eq!(store.messages().len(), 1);
        assert!(store.is_pending(&"u".into()));
        store.remove_pending_user_message(&"u".into());
        assert!(store.messages().is_empty());
    }

    #[test]
    fn history_rewrite_requires_snapshot_and_delete_is_terminal() {
        let mut store = ready_store();
        store.apply(event(11, "thread.reverted", json!({"threadId": "t-1"})));
        assert_eq!(store.status(), ThreadStatus::NeedsSnapshot);
        assert_eq!(store.resume_cursor(), None);
        store.apply(snapshot(12, json!([])));
        store.apply(event(
            13,
            "thread.deleted",
            json!({"threadId": "t-1", "deletedAt": "x"}),
        ));
        assert_eq!(store.status(), ThreadStatus::Deleted);
    }

    #[test]
    fn bad_payload_advances_cursor_and_counts() {
        let mut store = ready_store();
        store.apply(event(11, "thread.session-set", json!({"threadId": "t-1"})));
        assert_eq!(store.cursor(), Some(11));
        assert_eq!(store.decode_errors, 1);
    }

    proptest! {
        /// Streaming deltas then a completion always settle on the completion
        /// text, and replaying the same events again changes nothing.
        #[test]
        fn deltas_settle_and_replay_is_idempotent(deltas in proptest::collection::vec("[a-z ]{0,6}", 1..12)) {
            let mut store = ready_store();
            let mut items = Vec::new();
            let mut sequence = 11;
            for delta in &deltas {
                items.push(sent(sequence, "m", "assistant", delta, true));
                sequence += 1;
            }
            let full: String = deltas.concat();
            items.push(sent(sequence, "m", "assistant", &full, false));
            for item in items.clone() {
                store.apply(item);
            }
            prop_assert_eq!(&store.messages()[0].text, &full);
            prop_assert!(!store.messages()[0].streaming);
            let before = store.messages().to_vec();
            for item in items {
                prop_assert!(store.apply(item).is_empty());
            }
            prop_assert_eq!(store.messages(), before.as_slice());
        }
    }
}

//! `thread.message-sent` merge rules.

use synara_protocol::events::MessageSentPayload;
use synara_protocol::orchestration::{Message, MessageRole, TextSegment};

/// Builds a new message from a `thread.message-sent` payload.
pub fn message_from_payload(payload: &MessageSentPayload) -> Message {
    Message {
        id: payload.message_id.clone(),
        role: payload.role,
        text: payload.text.clone(),
        text_segments: None,
        turn_id: payload.turn_id.clone(),
        streaming: payload.streaming,
        created_at: payload.created_at.clone(),
        updated_at: payload.updated_at.clone(),
        extra: payload.extra.clone(),
    }
}

/// `mergeStreamingMessage`: returns `None` when nothing the client renders changed.
pub fn merge_message(
    existing: &Message,
    incoming: &MessageSentPayload,
    segment_sequence: u64,
) -> Option<Message> {
    let next_text = if existing.role == MessageRole::User
        && incoming.role == MessageRole::User
        && !incoming.streaming
    {
        incoming.text.clone()
    } else if incoming.streaming || incoming.text.is_empty() {
        format!("{}{}", existing.text, incoming.text)
    } else {
        // Non-streaming completions carry the authoritative accumulated text.
        incoming.text.clone()
    };

    let next_segments = advance_text_segments(
        existing.text_segments.as_deref(),
        SegmentInput {
            streaming: incoming.streaming,
            delta_text: &incoming.text,
            next_text: &next_text,
            segment_started_at: incoming.segment_started_at.as_deref(),
            segment_sequence: incoming.segment_sequence.unwrap_or(segment_sequence),
            updated_at: &incoming.updated_at,
        },
    );
    // The payload's `turnId` is `NullOr`, so it is always defined and always wins.
    let next_turn_id = incoming.turn_id.clone();

    if existing.text == next_text
        && existing.text_segments == next_segments
        && existing.streaming == incoming.streaming
        && existing.updated_at == incoming.updated_at
        && existing.turn_id == next_turn_id
    {
        return None;
    }

    let mut extra = existing.extra.clone();
    for (key, value) in &incoming.extra {
        extra.insert(key.clone(), value.clone());
    }
    Some(Message {
        id: existing.id.clone(),
        role: existing.role,
        text: next_text,
        text_segments: next_segments,
        turn_id: next_turn_id,
        streaming: incoming.streaming,
        created_at: existing.created_at.clone(),
        updated_at: incoming.updated_at.clone(),
        extra,
    })
}

struct SegmentInput<'a> {
    streaming: bool,
    delta_text: &'a str,
    next_text: &'a str,
    segment_started_at: Option<&'a str>,
    segment_sequence: u64,
    updated_at: &'a str,
}

fn segments_cover_text(segments: &[TextSegment], text: &str) -> bool {
    let mut collated = String::with_capacity(text.len());
    for segment in segments {
        collated.push_str(&segment.text);
    }
    collated == text
}

/// `advanceMessageTextSegments`: never invents segments the server did not send.
fn advance_text_segments(
    previous: Option<&[TextSegment]>,
    input: SegmentInput<'_>,
) -> Option<Vec<TextSegment>> {
    let previous = previous.filter(|segments| !segments.is_empty())?;
    if !input.streaming {
        if previous.len() < 2 || !segments_cover_text(previous, input.next_text) {
            return None;
        }
        let mut next = previous.to_vec();
        if let Some(tail) = next.last_mut() {
            tail.ended_at = input.updated_at.to_owned();
        }
        return Some(next);
    }
    let mut next = previous.to_vec();
    if let Some(started_at) = input.segment_started_at {
        let opened = TextSegment {
            sequence: input.segment_sequence,
            started_at: started_at.to_owned(),
            ended_at: input.updated_at.to_owned(),
            text: input.delta_text.to_owned(),
        };
        // A repeated boundary restarts that segment's displayed text.
        match next
            .iter()
            .position(|s| s.sequence == input.segment_sequence)
        {
            Some(index) => next[index] = opened,
            None => next.push(opened),
        }
        return Some(next);
    }
    if let Some(tail) = next.last_mut() {
        tail.text.push_str(input.delta_text);
        tail.ended_at = input.updated_at.to_owned();
    }
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synara_protocol::orchestration::Extra;

    pub(crate) fn payload(
        id: &str,
        role: MessageRole,
        text: &str,
        streaming: bool,
    ) -> MessageSentPayload {
        MessageSentPayload {
            thread_id: "t-1".into(),
            message_id: id.into(),
            role,
            text: text.to_owned(),
            segment_started_at: None,
            segment_sequence: None,
            turn_id: Some("turn-1".into()),
            streaming,
            created_at: "2026-11-09T00:00:00.000Z".to_owned(),
            updated_at: format!("2026-11-09T00:00:00.{:03}Z", text.len() % 1000),
            extra: Extra::new(),
        }
    }

    #[test]
    fn streaming_deltas_append_and_completion_is_authoritative() {
        let first = message_from_payload(&payload("m", MessageRole::Assistant, "Hel", true));
        let second =
            merge_message(&first, &payload("m", MessageRole::Assistant, "lo", true), 2).unwrap();
        assert_eq!(second.text, "Hello");
        assert!(second.streaming);
        let done = merge_message(
            &second,
            &payload("m", MessageRole::Assistant, "Hello!", false),
            3,
        )
        .unwrap();
        assert_eq!(done.text, "Hello!");
        assert!(!done.streaming);
    }

    #[test]
    fn empty_completion_keeps_streamed_text() {
        let streamed = message_from_payload(&payload("m", MessageRole::Assistant, "Hi", true));
        let done = merge_message(
            &streamed,
            &payload("m", MessageRole::Assistant, "", false),
            2,
        )
        .unwrap();
        assert_eq!(done.text, "Hi");
        assert!(!done.streaming);
    }

    #[test]
    fn user_message_echo_replaces_text() {
        let pending = message_from_payload(&payload("u", MessageRole::User, "draft", false));
        let echoed = merge_message(
            &pending,
            &payload("u", MessageRole::User, "final", false),
            2,
        )
        .unwrap();
        assert_eq!(echoed.text, "final");
    }

    #[test]
    fn identical_event_is_a_no_op() {
        let message = message_from_payload(&payload("u", MessageRole::User, "same", false));
        assert!(
            merge_message(&message, &payload("u", MessageRole::User, "same", false), 2).is_none()
        );
    }

    fn segment(sequence: u64, text: &str) -> TextSegment {
        TextSegment {
            sequence,
            started_at: "a".into(),
            ended_at: "a".into(),
            text: text.into(),
        }
    }

    #[test]
    fn segments_follow_deltas_and_boundaries() {
        let mut message = message_from_payload(&payload("m", MessageRole::Assistant, "One", true));
        message.text_segments = Some(vec![segment(1, "One")]);

        let delta = merge_message(
            &message,
            &payload("m", MessageRole::Assistant, " more", true),
            5,
        )
        .unwrap();
        assert_eq!(delta.text_segments.as_ref().unwrap()[0].text, "One more");

        let mut boundary = payload("m", MessageRole::Assistant, "Two", true);
        boundary.segment_started_at = Some("b".into());
        let opened = merge_message(&delta, &boundary, 9).unwrap();
        let segments = opened.text_segments.clone().unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1].sequence, 9);
        assert_eq!(opened.text, "One moreTwo");

        // Completion that still spells the segments keeps them.
        let done = merge_message(
            &opened,
            &payload("m", MessageRole::Assistant, "One moreTwo", false),
            10,
        )
        .unwrap();
        assert_eq!(done.text_segments.unwrap().len(), 2);

        // Completion that diverges drops them so the full text renders.
        let diverged = merge_message(
            &opened,
            &payload("m", MessageRole::Assistant, "Rewritten", false),
            10,
        )
        .unwrap();
        assert!(diverged.text_segments.is_none());
    }

    #[test]
    fn never_invents_segments() {
        let message = message_from_payload(&payload("m", MessageRole::Assistant, "x", true));
        let mut boundary = payload("m", MessageRole::Assistant, "y", true);
        boundary.segment_started_at = Some("b".into());
        assert!(merge_message(&message, &boundary, 2)
            .unwrap()
            .text_segments
            .is_none());
    }
}

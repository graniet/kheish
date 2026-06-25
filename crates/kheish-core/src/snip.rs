//! Lightweight prompt-view truncation for older messages.

use kheish_types::MessageRecord;

use crate::tokens::{rough_token_estimate_all, rough_token_estimate_message};

/// The result of one prompt-view snip pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnipResult {
    /// The number of oldest messages removed from the prompt projection.
    pub messages_removed: usize,
    /// The rough number of tokens freed by the removal.
    pub tokens_freed: usize,
    /// The offset of the first surviving message when available.
    pub new_head_offset: u64,
}

/// Removes the oldest removable messages from a prompt projection until it fits the budget.
pub fn snip_if_needed(
    messages: &[MessageRecord],
    token_budget: usize,
    keep_minimum: usize,
) -> SnipResult {
    let total_tokens = rough_token_estimate_all(messages);
    if total_tokens <= token_budget || messages.len() <= keep_minimum {
        return SnipResult::default();
    }

    let excess = total_tokens - token_budget;
    let mut freed = 0usize;
    let mut cut = 0usize;

    for message in messages {
        if freed >= excess || (messages.len() - cut) <= keep_minimum {
            break;
        }
        if message.pinned {
            break;
        }
        freed = freed.saturating_add(rough_token_estimate_message(message));
        cut += 1;
    }

    SnipResult {
        messages_removed: cut,
        tokens_freed: freed,
        new_head_offset: messages
            .get(cut)
            .and_then(|message| message.offset)
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use kheish_types::{MessageRecord, Role};

    use super::snip_if_needed;

    #[test]
    fn snip_noops_when_under_budget() {
        let messages = vec![MessageRecord::new("u1", Role::User, "hello")];
        assert_eq!(snip_if_needed(&messages, 100, 1).messages_removed, 0);
    }

    #[test]
    fn snip_removes_oldest_prefix() {
        let messages = vec![
            MessageRecord::new("u1", Role::User, "first").with_offset(1),
            MessageRecord::new("u2", Role::User, "second").with_offset(2),
            MessageRecord::new("u3", Role::User, "third").with_offset(3),
        ];
        let result = snip_if_needed(&messages, 6, 1);
        assert!(result.messages_removed >= 1);
        assert_eq!(result.new_head_offset, 2);
    }

    #[test]
    fn snip_stops_at_pinned_messages() {
        let messages = vec![
            MessageRecord::new("u1", Role::User, "first")
                .pinned()
                .with_offset(1),
            MessageRecord::new("u2", Role::User, "second").with_offset(2),
        ];
        let result = snip_if_needed(&messages, 1, 1);
        assert_eq!(result.messages_removed, 0);
        assert_eq!(result.tokens_freed, 0);
    }
}

//! Conversation compaction: asking the model to summarize earlier turns, and framing that summary
//! so the model can tell it apart from something the user wrote.

use crate::{ChatRequest, Message, Options, Role};

/// Sent to the model ahead of a summary, and stored with it.
pub const SUMMARY_HEADER: &str =
    "[Summary of the earlier conversation, which was compacted to save context]";

const INSTRUCTIONS: &str = "Summarize the conversation so far. The summary will replace these \
messages, so anything left out is lost. Keep the user's goals and constraints, decisions and their \
reasons, open questions, and exact details that would be costly to lose such as names, numbers, \
file paths, code and commands. Leave out pleasantries and anything that was superseded. Write \
concise markdown in the third person. Reply with only the summary and do not continue the \
conversation.";

/// Builds the request that summarizes `messages`, which should include any system prompt.
pub fn request(
    model: &str,
    mut messages: Vec<Message>,
    options: Options,
    focus: Option<&str>,
) -> ChatRequest {
    let instructions = match focus {
        Some(focus) => format!("{INSTRUCTIONS}\n\nThe user asked this summary to focus on: {focus}"),
        None => INSTRUCTIONS.to_string(),
    };
    messages.push(Message::new(Role::User, instructions));
    ChatRequest { model: model.to_string(), messages, options, schema: None, stream: true }
}

/// Tidies a model's reply into a summary, or returns None when nothing usable remains.
pub fn summary_from_reply(reply: &str) -> Option<String> {
    let summary = reply.rsplit("</think>").next().unwrap_or(reply).trim();
    (!summary.is_empty()).then(|| summary.to_string())
}

/// The summary inside stored message content, without the header sent to the model.
pub fn summary_body(content: &str) -> &str {
    content.strip_prefix(SUMMARY_HEADER).map_or(content, |rest| rest.trim_start_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instructions_come_last_and_keep_the_conversation_intact() {
        let messages = vec![
            Message::new(Role::System, "Be brief"),
            Message::new(Role::User, "hi"),
            Message::new(Role::Assistant, "hello"),
        ];
        let request = request("m", messages, Options::default(), None);
        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.messages[0].content, "Be brief");
        assert_eq!(request.messages[3].role, Role::User);
        assert!(request.messages[3].content.starts_with("Summarize the conversation"));
        assert!(!request.messages[3].content.contains("focus on"));
        assert!(request.stream);
    }

    #[test]
    fn focus_text_is_passed_to_the_model() {
        let request = request("m", Vec::new(), Options::default(), Some("keep all SQL schemas"));
        assert!(request.messages[0].content.ends_with("focus on: keep all SQL schemas"));
    }

    #[test]
    fn replies_are_tidied_and_empty_ones_are_rejected() {
        assert_eq!(summary_from_reply("  Notes\n").unwrap(), "Notes");
        assert_eq!(summary_from_reply("<think>hmm</think>\nNotes").unwrap(), "Notes");
        assert_eq!(summary_from_reply(" \n "), None);
        assert_eq!(summary_from_reply("<think>only thoughts</think>"), None);
    }

    #[test]
    fn the_header_is_removed_from_stored_summaries() {
        assert_eq!(summary_body(&format!("{SUMMARY_HEADER}\nNotes\nMore")), "Notes\nMore");
        assert_eq!(summary_body("No header"), "No header");
    }
}

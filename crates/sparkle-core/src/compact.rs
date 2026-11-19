//! Conversation compaction: reducing earlier turns via LLM summarization or local mechanical
//! methods, and framing the result so the model can tell it apart from something the user wrote.

use crate::context::{compact_count, estimate_tokens};
use crate::{ChatRequest, Message, Options, Role};

/// Sent to the model ahead of a summary, and stored with it.
pub const SUMMARY_HEADER: &str =
    "[Summary of the earlier conversation, which was compacted to save context]";

const SUMMARY_INSTRUCTIONS: &str = "Summarize the conversation so far. The summary will replace \
these messages, so anything left out is lost. Keep the user's goals and constraints, decisions \
and their reasons, open questions, and exact details that would be costly to lose such as names, \
numbers, file paths, code and commands. Leave out pleasantries and anything that was superseded. \
Write concise markdown in the third person. Reply with only the summary and do not continue the \
conversation.";

const HANDOFF_INSTRUCTIONS: &str = "Write a structured handoff document for the conversation so \
far. It will replace these messages so another turn can continue seamlessly without losing state. \
Use these markdown sections (omit any that do not apply):\n\
## Goal\n\
## Constraints & Preferences\n\
## Progress\n\
## Key Decisions\n\
## Open Questions\n\
## Critical Context\n\
Keep exact file paths, identifiers, numbers, commands and errors. Leave out pleasantries and \
superseded work. Reply with only the handoff document and do not continue the conversation.";

/// Minimum size (in bytes) of a fenced code block before `shake` elides it.
const SHAKE_FENCE_MIN_BYTES: usize = 320;
/// Maximum size (in bytes) of an older message body before `shake` trims its middle.
const SHAKE_MESSAGE_MAX_BYTES: usize = 1200;

/// Available compaction strategies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Method {
    /// Ask the model for a concise summary of earlier turns.
    #[default]
    Summary,
    /// Ask the model for a structured handoff document.
    Handoff,
    /// Local, zero-LLM elision of large code blocks and long older messages.
    Shake,
    /// Local, zero-LLM sliding window that drops older turns and keeps recent ones.
    Trim,
}

impl Method {
    pub const ALL: &[Method] = &[Method::Summary, Method::Handoff, Method::Shake, Method::Trim];

    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "summary" | "soft" => Some(Self::Summary),
            "handoff" => Some(Self::Handoff),
            "shake" => Some(Self::Shake),
            "trim" | "drop" => Some(Self::Trim),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Handoff => "handoff",
            Self::Shake => "shake",
            Self::Trim => "trim",
        }
    }

    pub fn is_local(self) -> bool {
        matches!(self, Self::Shake | Self::Trim)
    }

    pub fn divider_label(self) -> &'static str {
        match self {
            Self::Summary => "compacted (summary)",
            Self::Handoff => "compacted (handoff)",
            Self::Shake => "compacted (shake)",
            Self::Trim => "compacted (trim)",
        }
    }
}

/// Label shown in the transcript divider for a stored method name.
pub fn divider_label_for(method: Option<&str>) -> String {
    match method.map(str::trim).filter(|m| !m.is_empty()) {
        Some(name) => Method::parse(name)
            .map(|method| method.divider_label().to_string())
            .unwrap_or_else(|| format!("compacted ({name})")),
        None => "compacted".to_string(),
    }
}

/// Parses `/compact [METHOD] [FOCUS]` arguments. When the first word is not a known method,
/// defaults to `Method::Summary` and treats the entire argument as focus text.
pub fn parse_command(argument: &str) -> (Method, Option<&str>) {
    let trimmed = argument.trim();
    if trimmed.is_empty() {
        return (Method::Summary, None);
    }
    let (first, rest) = trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""));
    if let Some(method) = Method::parse(first) {
        let rest = rest.trim();
        (method, (!rest.is_empty()).then_some(rest))
    } else {
        (Method::Summary, Some(trimmed))
    }
}

/// Builds the request that summarizes `messages`, which should include any system prompt.
pub fn request(
    model: &str,
    messages: Vec<Message>,
    options: Options,
    focus: Option<&str>,
) -> ChatRequest {
    request_with_method(Method::Summary, model, messages, options, focus)
}

/// Builds a model-backed compaction request for `method` (`Summary` or `Handoff`).
pub fn request_with_method(
    method: Method,
    model: &str,
    mut messages: Vec<Message>,
    options: Options,
    focus: Option<&str>,
) -> ChatRequest {
    let base = match method {
        Method::Handoff => HANDOFF_INSTRUCTIONS,
        _ => SUMMARY_INSTRUCTIONS,
    };
    let instructions = match focus {
        Some(focus) => format!("{base}\n\nThe user asked this summary to focus on: {focus}"),
        None => base.to_string(),
    };
    messages.push(Message::new(Role::User, instructions));
    ChatRequest { model: model.to_string(), messages, options, schema: None, stream: true }
}

/// Replaces large fenced code blocks in `text` with short placeholders.
fn elide_fenced_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence: Option<(char, usize, String)> = None;
    let mut block_buf = String::new();

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some((marker, size, lang)) = &in_fence {
            block_buf.push_str(line);
            let count = trimmed.chars().take_while(|c| *c == *marker).count();
            if count >= *size && trimmed[count..].trim().is_empty() {
                if block_buf.len() >= SHAKE_FENCE_MIN_BYTES {
                    let tokens = compact_count(estimate_tokens(block_buf.len()));
                    let label = if lang.is_empty() { "code" } else { lang.as_str() };
                    out.push_str(&format!("[Elided ~{tokens} tokens of {label}]\n"));
                } else {
                    out.push_str(&block_buf);
                }
                block_buf.clear();
                in_fence = None;
            }
            continue;
        }

        let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        if let Some(marker) = marker {
            let count = trimmed.chars().take_while(|c| *c == marker).count();
            if count >= 3 {
                let lang = trimmed[count..].trim().split_whitespace().next().unwrap_or("").to_string();
                in_fence = Some((marker, count, lang));
                block_buf.clear();
                block_buf.push_str(line);
                continue;
            }
        }
        out.push_str(line);
    }

    if !block_buf.is_empty() {
        out.push_str(&block_buf);
    }
    out
}

/// Truncates a long message by keeping its head and tail around an elision marker.
fn truncate_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let head_budget = (max_bytes * 6) / 10;
    let tail_budget = max_bytes - head_budget;
    let head_end = text.floor_char_boundary(head_budget);
    let tail_start = text.ceil_char_boundary(text.len().saturating_sub(tail_budget));
    if tail_start <= head_end {
        return text.to_string();
    }
    let dropped = compact_count(estimate_tokens(tail_start - head_end));
    format!(
        "{}\n[... elided ~{dropped} tokens ...]\n{}",
        text[..head_end].trim_end(),
        text[tail_start..].trim_start()
    )
}

fn format_transcript_message(message: &Message, body: &str) -> String {
    let speaker = match message.role {
        Role::User => "User",
        Role::Assistant => "Assistant",
        Role::System => "System",
    };
    format!("**{speaker}:** {}", body.trim())
}

/// Local mechanical compaction (`shake`): elides large fenced blocks and verbose passages in older
/// messages while keeping the most recent `keep_recent` non-system messages intact.
pub fn shake(messages: &[Message], keep_recent: usize) -> Option<String> {
    let non_system: Vec<&Message> = messages.iter().filter(|m| m.role != Role::System).collect();
    if non_system.is_empty() {
        return None;
    }
    let protect = keep_recent.min(non_system.len().saturating_sub(1));
    let split = non_system.len() - protect;
    let mut parts = Vec::with_capacity(non_system.len());
    for (index, message) in non_system.iter().enumerate() {
        let body = if index < split {
            let without_fences = elide_fenced_blocks(&message.content);
            truncate_middle(&without_fences, SHAKE_MESSAGE_MAX_BYTES)
        } else {
            message.content.clone()
        };
        parts.push(format_transcript_message(message, &body));
    }
    let combined = parts.join("\n\n");
    (!combined.trim().is_empty()).then_some(combined)
}

/// Local sliding-window compaction (`trim`): drops older messages and keeps the last `keep_recent`
/// non-system messages verbatim.
pub fn trim(messages: &[Message], keep_recent: usize) -> Option<String> {
    let non_system: Vec<&Message> = messages.iter().filter(|m| m.role != Role::System).collect();
    let keep = keep_recent.max(1);
    if non_system.len() <= keep {
        return None;
    }
    let dropped = non_system.len() - keep;
    let kept = &non_system[dropped..];
    let mut parts = Vec::with_capacity(kept.len() + 1);
    parts.push(format!(
        "_Dropped {dropped} earlier message{} to free context; keeping the latest {} below._",
        if dropped == 1 { "" } else { "s" },
        kept.len(),
    ));
    for message in kept {
        parts.push(format_transcript_message(message, &message.content));
    }
    Some(parts.join("\n\n"))
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
    fn handoff_method_requests_structured_sections() {
        let request = request_with_method(
            Method::Handoff,
            "m",
            Vec::new(),
            Options::default(),
            Some("preserve API endpoints"),
        );
        assert!(request.messages[0].content.contains("## Goal"));
        assert!(request.messages[0].content.contains("## Key Decisions"));
        assert!(request.messages[0].content.ends_with("focus on: preserve API endpoints"));
    }

    #[test]
    fn command_parser_recognizes_methods_and_defaults_to_summary() {
        assert_eq!(parse_command(""), (Method::Summary, None));
        assert_eq!(parse_command("keep SQL"), (Method::Summary, Some("keep SQL")));
        assert_eq!(parse_command("summary keep SQL"), (Method::Summary, Some("keep SQL")));
        assert_eq!(parse_command("handoff"), (Method::Handoff, None));
        assert_eq!(parse_command("handoff focus here"), (Method::Handoff, Some("focus here")));
        assert_eq!(parse_command("shake"), (Method::Shake, None));
        assert_eq!(parse_command("trim 4"), (Method::Trim, Some("4")));
    }

    #[test]
    fn shake_elides_large_fenced_blocks_and_keeps_recent_messages() {
        let big_code = format!("Here is code:\n```rust\n{}\n```\nDone.", "let x = 1;\n".repeat(60));
        let messages = vec![
            Message::new(Role::User, "write rust"),
            Message::new(Role::Assistant, big_code),
            Message::new(Role::User, "latest question"),
            Message::new(Role::Assistant, "latest answer"),
        ];
        let shaken = shake(&messages, 2).unwrap();
        assert!(shaken.contains("[Elided ~"), "{shaken}");
        assert!(shaken.contains("of rust]"), "{shaken}");
        assert!(shaken.contains("latest question"));
        assert!(shaken.contains("latest answer"));
    }

    #[test]
    fn trim_drops_older_messages_and_keeps_the_recent_tail() {
        let messages = vec![
            Message::new(Role::User, "old 1"),
            Message::new(Role::Assistant, "old 2"),
            Message::new(Role::User, "recent 1"),
            Message::new(Role::Assistant, "recent 2"),
        ];
        let trimmed = trim(&messages, 2).unwrap();
        assert!(!trimmed.contains("old 1"));
        assert!(trimmed.contains("Dropped 2 earlier messages"));
        assert!(trimmed.contains("recent 1"));
        assert!(trimmed.contains("recent 2"));
        assert!(trim(&messages[..2], 2).is_none());
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

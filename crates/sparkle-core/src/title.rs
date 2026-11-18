//! Conversation titles. Titles reach the terminal's title bar, so anything derived from a
//! prompt or a model reply has control characters removed first.

use crate::{ChatRequest, Message, Options, Role};

const MAX_CHARS: usize = 48;
const MAX_PROMPT_CHARS: usize = 2000;
const INSTRUCTIONS: &str = "Write a short title for a conversation that starts with the user's \
message. Use three to six words in sentence case. Reply with only the title, with no quotes and \
no final punctuation. Do not answer the message.";

/// Removes control characters and collapses whitespace to single spaces.
pub fn sanitize(text: &str) -> String {
    let visible: String =
        text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    visible.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shorten(text: &str) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_CHARS - 1).collect();
    let kept = match cut.rfind(' ') {
        Some(space) if space > MAX_CHARS / 2 => &cut[..space],
        _ => cut.as_str(),
    };
    format!("{}…", kept.trim_end())
}

/// An immediate title taken from the first line of the user's message.
pub fn from_prompt(prompt: &str) -> String {
    let line = prompt.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
    shorten(&sanitize(line))
}

/// Tidies a model's reply into a title, or returns None when nothing usable remains.
pub fn from_model(reply: &str) -> Option<String> {
    let reply = reply.rsplit("</think>").next().unwrap_or(reply);
    let line = reply.lines().find(|line| !line.trim().is_empty())?;
    let line = sanitize(line).replace(['*', '`', '#'], "");
    let line = line.trim();
    let line = match line.get(..6) {
        Some(prefix) if prefix.eq_ignore_ascii_case("title:") => &line[6..],
        _ => line,
    };
    let line = line
        .trim_matches(|c: char| c.is_whitespace() || "\"'“”‘’".contains(c))
        .trim_end_matches(['.', '!', ':', ';', ','])
        .trim();
    (!line.is_empty()).then(|| shorten(line))
}

/// Builds the background request that asks the model to name the conversation.
pub fn request(model: &str, prompt: &str) -> ChatRequest {
    let excerpt: String = prompt.chars().take(MAX_PROMPT_CHARS).collect();
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message::new(Role::System, INSTRUCTIONS), Message::new(Role::User, excerpt)],
        options: Options::default(),
        schema: None,
        stream: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_titles_use_the_first_real_line() {
        assert_eq!(from_prompt("\n\n  Fix   my\tparser \nmore detail"), "Fix my parser");
    }

    #[test]
    fn long_prompts_are_shortened_at_a_word() {
        let title = from_prompt(&"word ".repeat(40));
        assert!(title.chars().count() <= MAX_CHARS, "{title}");
        assert!(title.ends_with("word…"), "{title}");
    }

    #[test]
    fn long_unbroken_text_is_still_shortened() {
        assert_eq!(from_prompt(&"x".repeat(100)).chars().count(), MAX_CHARS);
    }

    #[test]
    fn escape_sequences_cannot_reach_the_terminal() {
        let hostile = "safe\u{1b}]0;owned\u{7}\u{9b}31m title";
        for title in [from_prompt(hostile), from_model(hostile).unwrap()] {
            assert!(!title.chars().any(char::is_control), "{title:?}");
        }
    }

    #[test]
    fn model_replies_are_tidied() {
        assert_eq!(from_model("\"Rust parser bug.\"").unwrap(), "Rust parser bug");
        assert_eq!(from_model("**Title:** Sourdough basics\nextra").unwrap(), "Sourdough basics");
        assert_eq!(from_model("<think>hmm</think>\nTrip to Oslo").unwrap(), "Trip to Oslo");
    }

    #[test]
    fn empty_model_replies_are_rejected() {
        assert_eq!(from_model(""), None);
        assert_eq!(from_model("  \n \"\" "), None);
    }

    #[test]
    fn the_request_is_limited_and_carries_only_the_prompt() {
        let request = request("m", &"a".repeat(MAX_PROMPT_CHARS + 50));
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[1].content.len(), MAX_PROMPT_CHARS);
    }
}

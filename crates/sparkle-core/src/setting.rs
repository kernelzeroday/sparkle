//! The short note that tells the model where she is. A system prompt chosen from the library is
//! usually written for some other product, and describes features (web search, artifacts,
//! connectors, memory of past chats) that sparkle does not have, so she would otherwise offer
//! them. Worded to read correctly wherever it sits relative to the chosen prompt.

/// What the model is told about sparkle. `journal` says whether the journal is on, so that the
/// note never mentions memory the conversation does not have.
pub fn note(today: &str, journal: bool) -> String {
    let memory = if journal {
        "The only thing that carries from one conversation to the next is your journal, described \
         in this prompt."
    } else {
        "Nothing carries over from one conversation to the next."
    };
    format!(
        "# Where you are\n\
         This conversation is happening in sparkle, a terminal chat app the person runs on their \
         own computer. Today's date is {today}. If other instructions in this prompt describe a \
         particular product or app, such as its web search, file creation, artifacts, connectors, \
         image or video generation, or memory of past chats, those features are not available \
         here, and there are no tools to call. Answer from what you know. The person reads your replies as \
         Markdown in their terminal and can stop, retry, or edit them. Once the conversation \
         fills half of your context window, the person's messages end with a short line such as \
         [sparkle: about 70% of your context window is in use]; it comes from the app, not from \
         them. When it runs high, the person can free space by typing /compact, which you can \
         suggest. {memory}"
    )
}

/// Closes the system prompt when a chosen prompt describes another product. The last words of a
/// long prompt carry the most weight, so this keeps a large vendor prompt from burying the note.
pub const REMINDER: &str = "Reminder: you are in sparkle, a terminal chat. No tools, web search, \
    image or video generation, or features of other apps are available. Reply in plain Markdown, \
    with no tool calls, citation markers, or channel tags.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_note_names_the_date_and_only_promises_memory_that_exists() {
        let with = note("2026-11-19", true);
        assert!(with.contains("Today's date is 2026-11-19."));
        assert!(with.contains("your journal"));
        let without = note("2026-11-19", false);
        assert!(!without.contains("journal"));
        assert!(without.ends_with("Nothing carries over from one conversation to the next."));
        assert!(with.contains("[sparkle: about 70% of your context window is in use]"));
        assert!(with.contains("/compact"));
    }
}

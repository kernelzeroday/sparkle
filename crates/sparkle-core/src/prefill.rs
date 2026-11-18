//! Some servers ignore a prefill and start the answer again from the beginning. This filter
//! holds back the start of a reply until it can tell, then drops the repeated prefill.

#[derive(Debug, Default)]
pub struct PrefillFilter {
    prefill: String,
    ends_with_space: bool,
    pending: String,
    decided: bool,
}

impl PrefillFilter {
    pub fn new(prefill: &str) -> Self {
        let trimmed = prefill.trim();
        Self {
            prefill: trimmed.to_string(),
            ends_with_space: prefill.ends_with(char::is_whitespace),
            pending: String::new(),
            decided: trimmed.is_empty(),
        }
    }

    /// Accepts streamed text and returns the part that is safe to show.
    pub fn push(&mut self, text: &str) -> String {
        if self.decided {
            return text.to_string();
        }
        self.pending.push_str(text);
        let candidate = self.pending.trim_start();
        if let Some(rest) = candidate.strip_prefix(self.prefill.as_str()) {
            self.decided = true;
            let rest = if self.ends_with_space { rest.trim_start() } else { rest };
            let rest = rest.to_string();
            self.pending.clear();
            return rest;
        }
        if self.prefill.starts_with(candidate) {
            return String::new();
        }
        self.decided = true;
        std::mem::take(&mut self.pending)
    }

    /// Returns anything still held back when the reply ends.
    pub fn finish(&mut self) -> String {
        self.decided = true;
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(prefill: &str, chunks: &[&str]) -> String {
        let mut filter = PrefillFilter::new(prefill);
        let mut output: String = chunks.iter().map(|chunk| filter.push(chunk)).collect();
        output.push_str(&filter.finish());
        output
    }

    #[test]
    fn a_true_continuation_is_untouched() {
        assert_eq!(run("The answer is", &[" 4", "."]), " 4.");
    }

    #[test]
    fn a_restarted_answer_loses_the_repeat() {
        assert_eq!(run("The answer is", &["The answer is 4."]), " 4.");
    }

    #[test]
    fn a_repeat_split_across_chunks_is_detected() {
        assert_eq!(run("The answer is", &["\n", "The an", "swer", " is", " 4."]), " 4.");
    }

    #[test]
    fn a_prefill_ending_in_a_space_keeps_single_spacing() {
        assert_eq!(run("Sure: ", &["Sure: hello"]), "hello");
    }

    #[test]
    fn a_partial_match_that_diverges_is_released_intact() {
        assert_eq!(run("The answer is", &["The ", "cat sat"]), "The cat sat");
    }

    #[test]
    fn a_reply_shorter_than_the_prefill_is_not_lost() {
        assert_eq!(run("The answer is", &["The"]), "The");
    }

    #[test]
    fn no_prefill_passes_everything_through() {
        assert_eq!(run("", &["  hello"]), "  hello");
    }
}

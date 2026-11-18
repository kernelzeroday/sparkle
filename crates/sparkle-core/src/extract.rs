//! Fenced code block extraction for `-x` and `--xl`.

fn fenced_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, Vec<&str>)> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let ticks = trimmed.chars().take_while(|c| *c == '`').count();
        match &mut open {
            None if ticks >= 3 => open = Some((ticks, Vec::new())),
            None => {}
            Some((fence, _)) if ticks >= *fence && trimmed[ticks..].trim().is_empty() => {
                let (_, lines) = open.take().expect("a block is open");
                blocks.push(lines.join("\n") + "\n");
            }
            Some((_, lines)) => lines.push(line),
        }
    }
    blocks
}

/// Returns the first fenced block, or the whole text when there is none.
pub fn first_block(text: &str) -> String {
    fenced_blocks(text).into_iter().next().unwrap_or_else(|| text.to_string())
}

/// Returns the last fenced block, or the whole text when there is none.
pub fn last_block(text: &str) -> String {
    fenced_blocks(text).pop().unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "intro\n```python\nprint('a')\n```\nmid\n````\n```\nb\n````\n";

    #[test]
    fn extracts_first_and_last() {
        assert_eq!(first_block(TEXT), "print('a')\n");
        assert_eq!(last_block(TEXT), "```\nb\n");
    }

    #[test]
    fn falls_back_to_full_text() {
        assert_eq!(first_block("no code"), "no code");
        assert_eq!(last_block("```\nunclosed"), "```\nunclosed");
    }
}

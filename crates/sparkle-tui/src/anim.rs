//! Terminal animations, driven by a frame counter so they stay deterministic in tests.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BAND: f32 = 4.0;

pub fn spinner(frame: u64) -> &'static str {
    SPINNER[(frame % SPINNER.len() as u64) as usize]
}

fn mix(low: u8, high: u8, amount: f32) -> u8 {
    (f32::from(low) + (f32::from(high) - f32::from(low)) * amount).round() as u8
}

/// Renders a label with a band of light sweeping across it.
pub fn shimmer(
    label: &str,
    frame: u64,
    low: (u8, u8, u8),
    high: (u8, u8, u8),
) -> Vec<Span<'static>> {
    let length = label.chars().count();
    let cycle = length as u64 + (BAND as u64) * 2;
    let position = (frame % cycle) as f32 - BAND;
    label
        .chars()
        .enumerate()
        .map(|(index, character)| {
            let distance = (index as f32 - position).abs();
            let amount = (1.0 - distance / BAND).clamp(0.0, 1.0);
            let color =
                Color::Rgb(mix(low.0, high.0, amount), mix(low.1, high.1, amount), mix(low.2, high.2, amount));
            Span::styled(character.to_string(), Style::new().fg(color).add_modifier(Modifier::BOLD))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shimmer_keeps_the_label_and_moves_over_time() {
        let low = (0, 0, 0);
        let high = (255, 255, 255);
        let first = shimmer("Thinking", 4, low, high);
        let later = shimmer("Thinking", 8, low, high);
        let text: String = first.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "Thinking");
        assert_ne!(first[0].style, later[0].style);
        assert_eq!(first[0].style.fg, Some(Color::Rgb(255, 255, 255)));
    }
}

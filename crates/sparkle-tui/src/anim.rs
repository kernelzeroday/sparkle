//! Terminal animations, driven by a frame counter so they stay deterministic in tests.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

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

/// The red, green and blue parts of a color, or `fallback` when it has none.
pub fn rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        Color::Rgb(red, green, blue) => (red, green, blue),
        _ => fallback,
    }
}

fn blend(low: (u8, u8, u8), high: (u8, u8, u8), amount: f32) -> Color {
    let amount = amount.clamp(0.0, 1.0);
    Color::Rgb(mix(low.0, high.0, amount), mix(low.1, high.1, amount), mix(low.2, high.2, amount))
}

/// Brightness rising and falling once per `period` frames, offset by `phase`.
fn pulse(frame: u64, period: u64, phase: u64) -> f32 {
    let position = ((frame + phase) % period) as f32 / period as f32;
    1.0 - (position * 2.0 - 1.0).abs()
}

/// Where the small stars around the sparkle sit, as fractions of the canvas, with their phases.
const TWINKLES: [(f32, f32, u64); 7] = [
    (0.06, 0.12, 0),
    (0.90, 0.22, 11),
    (0.14, 0.80, 23),
    (0.84, 0.86, 5),
    (0.28, 0.02, 17),
    (0.70, 0.98, 29),
    (0.98, 0.55, 8),
];
const TWINKLE_PERIOD: u64 = 34;
const SHIMMER_PERIOD: u64 = 60;

/// Rows the sparkle needs at `radius`.
pub fn sparkle_height(radius: u16) -> u16 {
    radius + 1
}

/// A four-point star with light sweeping across it, a slow breath, and small stars twinkling
/// around it. Half blocks make each cell two square pixels; `ascii` keeps to plain characters.
pub fn sparkle(
    radius: u16,
    frame: u64,
    ascii: bool,
    low: (u8, u8, u8),
    high: (u8, u8, u8),
) -> Vec<Line<'static>> {
    let margin = 4usize;
    let star_width = usize::from(radius) * 2 + 1;
    let width = star_width + margin * 2;
    let rows = usize::from(sparkle_height(radius));
    let center_x = width as f32 / 2.0;
    let center_y = rows as f32; // in half-cell pixels
    let breath = pulse(frame, 50, 0);
    let reach = f32::from(radius) * (0.92 + 0.08 * breath) + 0.5;
    // A band of light moving diagonally, with a pause between sweeps.
    let sweep = (frame % SHIMMER_PERIOD) as f32 / SHIMMER_PERIOD as f32 * 2.2 - 0.6;

    // How bright a pixel is, or None outside the star.
    let pixel = |x: f32, y: f32| -> Option<f32> {
        let dx = (x - center_x).abs() / reach;
        let dy = (y - center_y).abs() / reach;
        let shape = dx.powf(0.7) + dy.powf(0.7);
        if shape > 1.0 {
            return None;
        }
        let core = (1.0 - shape).powf(1.6);
        let diagonal = ((x / width as f32) + (y / (rows * 2) as f32)) / 2.0;
        let band = (1.0 - (diagonal - sweep).abs() * 6.0).max(0.0);
        Some((0.15 + core * 0.85 + band * 0.6).min(1.0))
    };

    let mut grid: Vec<Vec<Span<'static>>> = (0..rows)
        .map(|row| {
            (0..width)
                .map(|column| {
                    let x = column as f32 + 0.5;
                    let top = pixel(x, row as f32 * 2.0 + 0.5);
                    let bottom = pixel(x, row as f32 * 2.0 + 1.5);
                    if ascii {
                        let brightness = match (top, bottom) {
                            (Some(a), Some(b)) => (a + b) / 2.0,
                            (Some(a), None) | (None, Some(a)) => a * 0.6,
                            (None, None) => return Span::raw(" "),
                        };
                        let glyph = match brightness {
                            b if b > 0.75 => "#",
                            b if b > 0.45 => "*",
                            b if b > 0.25 => "+",
                            _ => ".",
                        };
                        return Span::styled(glyph, Style::new().fg(blend(low, high, brightness)));
                    }
                    match (top, bottom) {
                        (None, None) => Span::raw(" "),
                        (Some(a), None) => Span::styled("▀", Style::new().fg(blend(low, high, a))),
                        (None, Some(b)) => Span::styled("▄", Style::new().fg(blend(low, high, b))),
                        (Some(a), Some(b)) => Span::styled(
                            "▀",
                            Style::new().fg(blend(low, high, a)).bg(blend(low, high, b)),
                        ),
                    }
                })
                .collect()
        })
        .collect();

    let glyphs: [&str; 4] = if ascii { [" ", ".", "+", "*"] } else { [" ", "·", "✧", "✦"] };
    for (fx, fy, phase) in TWINKLES {
        let column = ((fx * (width - 1) as f32).round() as usize).min(width - 1);
        let row = ((fy * (rows - 1) as f32).round() as usize).min(rows - 1);
        if grid[row][column].content != " " {
            continue;
        }
        let brightness = pulse(frame, TWINKLE_PERIOD, phase * 3);
        let level = ((brightness * 4.0) as usize).min(3);
        let style = Style::new().fg(blend(low, high, brightness));
        grid[row][column] = Span::styled(glyphs[level], style);
    }
    grid.into_iter().map(Line::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect()).collect()
    }

    #[test]
    fn sparkle_is_symmetric_star_that_twinkles() {
        let lines = sparkle(8, 0, false, (0, 0, 0), (255, 255, 255));
        assert_eq!(lines.len(), usize::from(sparkle_height(8)));
        let rows = text(&lines);
        let widths: Vec<usize> = rows.iter().map(|row| row.chars().count()).collect();
        assert!(widths.iter().all(|&width| width == widths[0]), "{rows:#?}");
        // The arms reach the middle of the top and bottom rows, and the widest row is the middle.
        let middle = widths[0] / 2;
        assert_ne!(rows[0].chars().nth(middle), Some(' '), "{rows:#?}");
        assert_ne!(rows[rows.len() - 1].chars().nth(middle), Some(' '), "{rows:#?}");
        let later = sparkle(8, 9, false, (0, 0, 0), (255, 255, 255));
        assert_ne!(lines, later);
    }

    #[test]
    fn ascii_sparkle_uses_plain_characters() {
        let rows = text(&sparkle(5, 3, true, (0, 0, 0), (255, 255, 255)));
        assert!(rows.iter().all(|row| row.is_ascii()), "{rows:#?}");
    }

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

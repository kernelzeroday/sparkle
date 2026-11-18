//! Drawing. Everything here reads application state and writes to a frame.

use sparkle_core::Role;
use sparkle_core::context::{
    self, Fit, compact_count, estimate_tokens, format_bytes, size_label, usage_label,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::anim;
use crate::app::{App, COMMANDS, Phase, Turn, matching_commands};
use crate::markdown::{self, wrap_spans};
use crate::picker::{Entry, Picker};
use crate::theme::{Icons, Theme};

const MAX_CONTENT_WIDTH: u16 = 110;
const MAX_INPUT_LINES: u16 = 8;

fn seconds(milliseconds: u64) -> String {
    format!("{:.1}s", milliseconds as f64 / 1000.0)
}

fn prefixed(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    prefix: Span<'static>,
    style: Style,
) {
    let available = width.saturating_sub(prefix.content.width()).max(8);
    for source in text.split('\n') {
        let wrapped = wrap_spans(&[Span::styled(source.to_string(), style)], available);
        if wrapped.is_empty() {
            lines.push(Line::from(prefix.clone()));
        }
        for row in wrapped {
            let mut spans = vec![prefix.clone()];
            spans.extend(row);
            lines.push(Line::from(spans));
        }
    }
}

fn reply_stats(turn: &Turn) -> String {
    let mut parts = Vec::new();
    if let Some(usage) = turn.usage {
        parts.push(format!("{} in", usage.input_tokens));
        parts.push(format!("{} out", usage.output_tokens));
        if let Some(duration) = turn.duration_ms.filter(|duration| *duration > 0) {
            let rate = usage.output_tokens as f64 * 1000.0 / duration as f64;
            parts.push(format!("{rate:.0} tok/s"));
        }
    }
    if let Some(duration) = turn.duration_ms {
        parts.push(seconds(duration));
    }
    parts.join(" · ")
}

fn attached_label(count: usize) -> String {
    format!("{count} file{} attached", if count == 1 { "" } else { "s" })
}

fn render_turn(
    turn: &Turn,
    width: u16,
    show_reasoning: bool,
    theme: &Theme,
    icons: &Icons,
) -> Vec<Line<'static>> {
    let columns = usize::from(width);
    let mut lines = Vec::new();
    match turn.role {
        None => {
            let bar = Span::styled("  ", Style::new());
            prefixed(&mut lines, &turn.text, columns, bar, Style::new().fg(theme.muted));
        }
        Some(_) if turn.summary => {
            lines.push(Line::from(Span::styled(
                "Summary of earlier conversation",
                Style::new().fg(theme.heading).add_modifier(Modifier::BOLD),
            )));
            lines.extend(markdown::render(&turn.text, width, theme, icons));
        }
        Some(Role::User | Role::System) => {
            let bar = Span::styled("▍ ", Style::new().fg(theme.user));
            prefixed(&mut lines, &turn.text, columns, bar.clone(), Style::new().fg(theme.text));
            if turn.attached_files > 0 {
                let label = attached_label(turn.attached_files);
                prefixed(&mut lines, &label, columns, bar, Style::new().fg(theme.muted));
            }
        }
        Some(Role::Assistant) => {
            let mut header = vec![Span::styled(
                format!("{} {}", icons.assistant, turn.model),
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            )];
            let stats = reply_stats(turn);
            if !stats.is_empty() {
                header.push(Span::styled(format!("  {stats}"), Style::new().fg(theme.faint)));
            }
            lines.push(Line::from(header));

            if !turn.reasoning.is_empty() {
                let chevron = if show_reasoning { icons.expanded } else { icons.collapsed };
                let label = turn.thought_ms.map_or_else(
                    || "Thinking".to_string(),
                    |thought| format!("Thought for {}", seconds(thought)),
                );
                lines.push(Line::from(Span::styled(
                    format!("{chevron} {} {label}", icons.thinking),
                    Style::new().fg(theme.muted),
                )));
                if show_reasoning {
                    let bar = Span::styled("│ ", Style::new().fg(theme.faint));
                    let style = Style::new().fg(theme.muted).add_modifier(Modifier::ITALIC);
                    prefixed(&mut lines, turn.reasoning.trim_end(), columns, bar, style);
                }
                lines.push(Line::default());
            }
            lines.extend(markdown::render(&turn.text, width, theme, icons));
            if turn.interrupted {
                lines.push(Line::from(Span::styled(
                    format!("{} Stopped", icons.stopped),
                    Style::new().fg(theme.muted),
                )));
            }
            if let Some(error) = &turn.error {
                let mark = Span::styled(format!("{} ", icons.error), Style::new().fg(theme.error));
                prefixed(&mut lines, error, columns, mark, Style::new().fg(theme.error));
                lines.push(Line::from(Span::styled(
                    "  /retry to try again",
                    Style::new().fg(theme.muted),
                )));
            }
        }
    }
    lines.push(Line::default());
    lines
}

fn fragment_bytes(app: &App) -> usize {
    app.fragments.iter().map(String::len).sum()
}

/// What this chat will send before the first message, one fact per line.
fn session_details(app: &App) -> Vec<String> {
    let mut details = Vec::new();
    if let Some(system) = &app.system {
        let size = size_label(system.len());
        details.push(match &app.persona {
            Some(persona) => format!("System prompt: {persona}, {size}"),
            None => format!("System prompt: {size}"),
        });
    }
    if !app.fragments.is_empty() {
        let label = attached_label(app.fragments.len());
        details.push(format!("{label}: {}", size_label(fragment_bytes(app))));
    }
    if let Some(prefill) = &app.prefill {
        details.push(format!("Prefill: {}", size_label(prefill.len())));
    }
    if let Some(window) = app.context_window {
        details.push(format!("Context window: {} tokens", compact_count(window)));
    }
    details
}

fn draw_welcome(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = &app.theme;
    let mut lines = vec![
        Line::from(Span::styled(
            format!("{}  sparkle", app.icons.assistant),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from(Span::styled(
            format!("Chatting with {}", app.model),
            Style::new().fg(theme.text),
        )),
        Line::from(Span::styled(
            "/help for commands · shift+tab for effort · ctrl+t for thinking",
            Style::new().fg(theme.muted),
        )),
    ];
    let details = session_details(app);
    if !details.is_empty() {
        lines.push(Line::default());
    }
    for detail in details {
        lines.push(Line::from(Span::styled(detail, Style::new().fg(theme.heading))));
    }
    let height = lines.len() as u16;
    let top = area.y + area.height.saturating_sub(height) / 2;
    let centered = Rect { y: top, height: height.min(area.height), ..area };
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), centered);
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if app.turns.is_empty() {
        draw_welcome(frame, area, app);
        return;
    }
    let width = area.width.saturating_sub(2).clamp(10, MAX_CONTENT_WIDTH);
    let (theme, icons, show) = (app.theme, app.icons, app.show_reasoning);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for turn in &mut app.turns {
        let fresh = turn
            .cache
            .as_ref()
            .is_some_and(|(cached_width, cached_show, _)| *cached_width == width && *cached_show == show);
        if !fresh {
            turn.cache = Some((width, show, render_turn(turn, width, show, &theme, &icons)));
        }
        if let Some((_, _, cached)) = &turn.cache {
            lines.extend_from_slice(cached);
        }
    }
    // While scrolled up, new lines must not drag the text being read out of view.
    if app.scroll > 0 {
        app.scroll += lines.len().saturating_sub(app.last_line_count);
    }
    app.last_line_count = lines.len();
    let height = usize::from(area.height);
    let furthest = lines.len().saturating_sub(height);
    app.scroll = app.scroll.min(furthest);
    let start = furthest - app.scroll;
    let visible: Vec<Line<'static>> = lines.into_iter().skip(start).take(height).collect();
    let inner = Rect { x: area.x + 1, width: area.width.saturating_sub(1), ..area };
    frame.render_widget(Paragraph::new(visible), inner);
}

fn activity_line(app: &App) -> Line<'static> {
    let theme = &app.theme;
    let muted = Style::new().fg(theme.muted);
    if app.exit_pending()
        && let Some(notice) = &app.notice
    {
        return Line::from(Span::styled(format!(" {notice}"), Style::new().fg(theme.heading)));
    }
    if app.busy() {
        let label = match (app.phase, app.prompt_progress) {
            (Phase::Compacting, Some(progress)) => format!("Compacting: {}", progress.label()),
            (Phase::Compacting, None) => "Compacting".to_string(),
            (Phase::Waiting, Some(progress)) => progress.label(),
            (Phase::Waiting, None) => "Connecting".to_string(),
            (Phase::Thinking, _) => "Thinking".to_string(),
            _ => "Writing".to_string(),
        };
        let mut spans = Vec::new();
        if app.animations {
            spans.push(Span::styled(
                format!(" {} ", anim::spinner(app.frame)),
                Style::new().fg(theme.accent),
            ));
            spans.extend(anim::shimmer(&label, app.frame, theme.shimmer_low, theme.shimmer_high));
        } else {
            spans.push(Span::styled(format!(" {} {label}", app.icons.thinking), muted));
        }
        let elapsed = app.started.map_or(0, |start| start.elapsed().as_millis() as u64);
        let mut detail = format!("  {}", seconds(elapsed));
        if app.phase == Phase::Thinking {
            let reasoning = app.turns.last().map_or(0, |turn| turn.reasoning.len());
            detail.push_str(&format!(" · ~{} tokens", reasoning / 4));
        }
        detail.push_str(" · esc to stop");
        spans.push(Span::styled(detail, muted));
        return Line::from(spans);
    }
    if let Some(notice) = &app.notice {
        return Line::from(Span::styled(format!(" {notice}"), Style::new().fg(theme.accent)));
    }
    if let Some(typed) = app.typing_command() {
        let cycling = app.completion.as_ref().filter(|state| state.current == typed);
        let seed = cycling.map_or(typed, |state| state.typed.as_str());
        let matches = matching_commands(seed);
        if let [only] = matches.as_slice() {
            let about = COMMANDS.iter().find(|(name, _)| name == only).map_or("", |(_, about)| *about);
            return Line::from(Span::styled(format!(" {only}  {about}"), muted));
        }
        let mut spans = vec![Span::raw(" ")];
        for (index, name) in matches.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("  "));
            }
            let selected = cycling.is_some_and(|state| state.current == *name);
            let style = if selected {
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                muted
            };
            spans.push(Span::styled(name.to_string(), style));
        }
        return Line::from(spans);
    }
    if let Some((fit, warning)) = app.context_warning() {
        let style = Style::new().fg(fit_color(fit, theme));
        return Line::from(Span::styled(format!(" {} {warning}", app.icons.error), style));
    }
    Line::default()
}

fn fit_color(fit: Fit, theme: &Theme) -> Color {
    match fit {
        Fit::Fits => theme.faint,
        Fit::Near => theme.heading,
        Fit::Over => theme.error,
    }
}

/// Context usage and token totals, dropping the totals first when space is short.
fn status_right(app: &App, room: usize) -> String {
    let context = app.context_window.map(|window| {
        let (used, estimated) = app.context_used();
        usage_label(used, window, estimated)
    });
    let usage = app.total_usage;
    let totals = (usage.input_tokens + usage.output_tokens > 0)
        .then(|| format!("{} in · {} out", usage.input_tokens, usage.output_tokens));
    let full = match (&context, &totals) {
        (Some(context), Some(totals)) => format!("{context} · {totals} "),
        (Some(text), None) | (None, Some(text)) => format!("{text} "),
        (None, None) => String::new(),
    };
    match context {
        Some(context) if full.width() >= room => format!("{context} "),
        _ => full,
    }
}

fn status_line(app: &App, width: u16) -> Line<'static> {
    let theme = &app.theme;
    let icons = &app.icons;
    let muted = Style::new().fg(theme.muted);
    let mut left = vec![Span::styled(
        format!(" {} {}", icons.model, app.model),
        Style::new().fg(theme.accent),
    )];
    let effort = app.options.effort.as_deref().unwrap_or("auto");
    left.push(Span::styled(format!("   {} {effort}", icons.effort), muted));
    let heading = Style::new().fg(theme.heading);
    if let Some(persona) = &app.persona {
        left.push(Span::styled(format!("   {} {persona}", icons.persona), muted));
    }
    if let Some(system) = &app.system {
        let icon = if app.persona.is_some() { String::new() } else { format!("{} ", icons.persona) };
        left.push(Span::styled(format!("   {icon}sys {}", size_label(system.len())), heading));
    }
    if let Some(prefill) = &app.prefill {
        let size = format_bytes(prefill.len());
        left.push(Span::styled(format!("   prefill set ({size})"), heading));
    }
    if !app.fragments.is_empty() {
        let label = attached_label(app.fragments.len());
        let size = format_bytes(fragment_bytes(app));
        left.push(Span::styled(format!("   {label} ({size})"), heading));
    }
    let used: usize = left.iter().map(|span| span.content.width()).sum();
    let right = status_right(app, usize::from(width).saturating_sub(used));
    let gap = usize::from(width).saturating_sub(used + right.width());
    if gap > 0 {
        left.push(Span::raw(" ".repeat(gap)));
        let fit = app.context_warning().map_or(Fit::Fits, |(fit, _)| fit);
        left.push(Span::styled(right, Style::new().fg(fit_color(fit, theme))));
    }
    Line::from(left)
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let input_lines = (app.input.lines().len() as u16).clamp(1, MAX_INPUT_LINES);
    let [transcript, activity, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(input_lines + 2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_transcript(frame, transcript, app);
    frame.render_widget(Paragraph::new(activity_line(app)), activity);
    frame.render_widget(&app.input, input);
    frame.render_widget(Paragraph::new(status_line(app, status.width)), status);
    if let Some(picker) = &app.picker {
        draw_picker(frame, app, picker);
    }
}

/// Below this the browser has no room for its list.
const PICKER_MIN: (u16, u16) = (24, 9);
/// The preview appears once the browser is this wide.
const PREVIEW_MIN_WIDTH: u16 = 92;

/// Cuts text to a display width, marking the cut.
fn fit_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut used = 0;
    for character in text.chars() {
        let size = character.width().unwrap_or(0);
        if used + size + 1 > width {
            break;
        }
        kept.push(character);
        used += size;
    }
    if width > 0 {
        kept.push('…');
    }
    kept
}

/// How a prompt of this size would fit, as a short column for its row.
fn fit_badge(app: &App, tokens: u64) -> Option<(Fit, String)> {
    let window = app.context_window?;
    let fit = context::fit(tokens, window, app.warn_percent);
    let percent = context::percent_of(tokens, window);
    Some((
        fit,
        match fit {
            Fit::Fits => format!("{percent}%"),
            Fit::Near => format!("{percent}% near"),
            Fit::Over => format!("{percent}% over"),
        },
    ))
}

/// The vendor tabs, starting late enough that the current one is on screen.
fn picker_tabs(picker: &Picker, theme: &Theme, width: usize) -> Line<'static> {
    let mut start = 0;
    let span_of = |range: std::ops::RangeInclusive<usize>| -> usize {
        picker.groups[range].iter().map(|name| name.width() + 2).sum()
    };
    while start < picker.group && span_of(start..=picker.group) > width {
        start += 1;
    }
    let mut spans = Vec::new();
    for (index, name) in picker.groups.iter().enumerate().skip(start) {
        let style = if index == picker.group {
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::new().fg(theme.muted)
        };
        spans.push(Span::styled((*name).to_string(), style));
        spans.push(Span::raw("  "));
    }
    Line::from(spans)
}

fn picker_row(app: &App, entry: &Entry, selected: bool, width: usize) -> Line<'static> {
    let theme = &app.theme;
    let icons = &app.icons;
    let active = match &app.persona {
        Some(persona) => *persona == entry.name,
        None => entry.name == sparkle_core::library::OFF && app.system.is_none(),
    };
    let is_default = app.default_prompt.as_deref() == Some(entry.name.as_str());
    let background = if selected { Style::new().bg(theme.code_bg) } else { Style::new() };
    let pointer = if selected { icons.collapsed } else { " " };
    let marks = format!(
        "{pointer}{}{} ",
        if active { icons.active } else { " " },
        if is_default { icons.starred } else { " " },
    );

    let tokens = app.tokens_with_prompt(entry.text.len());
    let badge = fit_badge(app, tokens);
    let badge_text = badge.as_ref().map_or("", |(_, text)| text.as_str());
    let size = if entry.text.is_empty() {
        String::new()
    } else {
        format!("~{}", compact_count(estimate_tokens(entry.text.len())))
    };
    let tail = format!("{size:>6} {badge_text:>9}");
    let name_width = width.saturating_sub(marks.width() + tail.width() + 1);
    let name = fit_width(&entry.name, name_width);
    let padding = " ".repeat(name_width.saturating_sub(name.width()) + 1);

    let text = if selected { theme.text } else { theme.muted };
    let badge_color = badge.as_ref().map_or(theme.faint, |(fit, _)| fit_color(*fit, theme));
    Line::from(vec![
        Span::styled(marks, background.fg(theme.accent)),
        Span::styled(name + &padding, background.fg(text)),
        Span::styled(format!("{size:>6} "), background.fg(theme.faint)),
        Span::styled(format!("{badge_text:>9}"), background.fg(badge_color)),
    ])
}

/// Facts about the highlighted prompt, led by how it fits this model.
fn picker_detail(app: &App, entry: &Entry) -> Line<'static> {
    let theme = &app.theme;
    let mut facts = vec![entry.title.clone()];
    if !entry.note.is_empty() {
        facts.push(entry.note.clone());
    }
    if !entry.text.is_empty() {
        facts.push(size_label(entry.text.len()));
    }
    let tokens = app.tokens_with_prompt(entry.text.len());
    let warning =
        app.context_window.and_then(|window| context::warning(tokens, window, app.warn_percent));
    let mut spans = Vec::new();
    match warning {
        Some((fit, text)) => {
            let style = Style::new().fg(fit_color(fit, theme)).add_modifier(Modifier::BOLD);
            spans.push(Span::styled(format!("{text} · "), style));
        }
        None if app.context_window.is_none() => {
            facts.push("context size unknown for this model".to_string());
        }
        None => {}
    }
    spans.push(Span::styled(facts.join(" · "), Style::new().fg(theme.muted)));
    Line::from(spans)
}

fn picker_keys(picker: &Picker, theme: &Theme) -> Line<'static> {
    let keys = if picker.path.is_some() {
        "tab complete · enter use · ctrl+d use and save as default · esc back"
    } else {
        "↑↓ move · ←→ vendor · type to filter · enter use · ctrl+d default · ctrl+o file · esc close"
    };
    Line::from(Span::styled(keys, Style::new().fg(theme.faint)))
}

fn draw_picker(frame: &mut Frame<'_>, app: &App, picker: &Picker) {
    let theme = &app.theme;
    let screen = frame.area();
    if screen.width < PICKER_MIN.0 || screen.height < PICKER_MIN.1 {
        return;
    }
    let width = screen.width.saturating_sub(4).min(130);
    let height = screen.height.saturating_sub(2).min(40);
    let area = Rect {
        x: screen.x + (screen.width - width) / 2,
        y: screen.y + (screen.height - height) / 2,
        width,
        height,
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .title(Span::styled(" System prompts ", Style::new().fg(theme.accent)));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [tabs, search, body, detail, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let tab_line = picker_tabs(picker, theme, usize::from(tabs.width));
    frame.render_widget(Paragraph::new(tab_line), tabs);

    let search_line = match &picker.path {
        Some(path) => Line::from(vec![
            Span::styled("File: ", Style::new().fg(theme.heading)),
            Span::styled(format!("{path}▏"), Style::new().fg(theme.text)),
        ]),
        None if picker.filter.is_empty() => {
            Line::from(Span::styled("Type to filter", Style::new().fg(theme.faint)))
        }
        None => Line::from(vec![
            Span::styled("Filter: ", Style::new().fg(theme.heading)),
            Span::styled(format!("{}▏", picker.filter), Style::new().fg(theme.text)),
        ]),
    };
    frame.render_widget(Paragraph::new(search_line), search);

    let (list, preview) = if body.width >= PREVIEW_MIN_WIDTH {
        let [list, preview] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(body);
        (list, Some(preview))
    } else {
        (body, None)
    };

    let visible = picker.visible();
    let rows = usize::from(list.height);
    let first = picker.cursor.saturating_sub(rows / 2).min(visible.len().saturating_sub(rows));
    let lines: Vec<Line<'static>> = visible
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(position, &index)| {
            let selected = position == picker.cursor;
            picker_row(app, &picker.entries[index], selected, usize::from(list.width))
        })
        .collect();
    if lines.is_empty() {
        let empty = Span::styled("No prompts match. ←→ switches vendor", Style::new().fg(theme.muted));
        frame.render_widget(Paragraph::new(Line::from(empty)), list);
    } else {
        frame.render_widget(Paragraph::new(lines), list);
    }

    let selected = picker.selected();
    if let (Some(preview), Some(entry)) = (preview, selected) {
        let pane = Block::new().borders(Borders::LEFT).border_style(Style::new().fg(theme.faint));
        let text_area = pane.inner(preview);
        frame.render_widget(pane, preview);
        // Long prompts are cut here because only the first screenful can be seen.
        let shown: Vec<Line<'static>> = entry
            .text
            .lines()
            .take(usize::from(text_area.height))
            .map(|line| Line::from(line.to_string()))
            .collect();
        let paragraph =
            Paragraph::new(shown).style(Style::new().fg(theme.muted)).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, text_area);
    }

    let detail_line = match (&picker.error, selected) {
        (Some(error), _) => Line::from(Span::styled(error.clone(), Style::new().fg(theme.error))),
        (None, Some(entry)) => picker_detail(app, entry),
        (None, None) => Line::default(),
    };
    frame.render_widget(Paragraph::new(detail_line), detail);
    frame.render_widget(Paragraph::new(picker_keys(picker, theme)), keys);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{app_with, drain, reply_events, type_text};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use sparkle_core::{PromptProgress, StreamEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn enter(app: &mut App) {
        app.on_terminal(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[tokio::test]
    async fn welcome_screen_names_the_model() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        let screen = screen(&mut app, 80, 24);
        assert!(screen.contains("Chatting with mock"), "{screen}");
        assert!(screen.contains("@ mock"), "{screen}");
    }

    #[tokio::test]
    async fn reasoning_is_shown_by_default_and_can_be_collapsed() {
        let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
        type_text(&mut app, "hi");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;

        let expanded = screen(&mut app, 80, 24);
        assert!(expanded.contains("Hello there"), "{expanded}");
        assert!(expanded.contains("│ pondering"), "{expanded}");

        app.on_terminal(Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)));
        let collapsed = screen(&mut app, 80, 24);
        assert!(collapsed.contains("Thought for"), "{collapsed}");
        assert!(!collapsed.contains("pondering"), "{collapsed}");
    }

    #[tokio::test]
    async fn reasoning_streams_live_while_the_model_thinks() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        enter(&mut app);
        app.turns.last_mut().unwrap().reasoning = "live thought".into();
        let screen = screen(&mut app, 80, 24);
        assert!(screen.contains("│ live thought"), "{screen}");
    }

    #[tokio::test]
    async fn hint_highlights_the_command_tab_selected() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "/e");
        app.on_terminal(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        let screen = screen(&mut app, 80, 24);
        assert!(screen.contains("/effort  /edit"), "{screen}");
    }

    #[tokio::test]
    async fn activity_line_animates_while_thinking() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        enter(&mut app);
        let first = screen(&mut app, 80, 24);
        assert!(first.contains("Connecting"), "{first}");
        assert!(first.contains("esc to stop"), "{first}");
        app.frame += 1;
        assert_ne!(first, screen(&mut app, 80, 24));
    }

    #[tokio::test]
    async fn long_transcripts_stay_pinned_to_the_bottom_and_scroll() {
        let long = (0..60).map(|n| format!("line{n}\n\n")).collect::<String>();
        let (mut app, mut receiver, _provider) = app_with(vec![StreamEvent::Text(long)], None);
        type_text(&mut app, "hi");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;

        let bottom = screen(&mut app, 60, 20);
        assert!(bottom.contains("line59"), "{bottom}");
        app.on_terminal(Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
        let scrolled = screen(&mut app, 60, 20);
        assert!(!scrolled.contains("line59"), "{scrolled}");
    }

    #[tokio::test]
    async fn reading_history_stays_put_while_a_reply_streams() {
        let long = (0..60).map(|n| format!("line{n}\n\n")).collect::<String>();
        let (mut app, mut receiver, _provider) = app_with(vec![StreamEvent::Text(long)], None);
        type_text(&mut app, "hi");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;
        screen(&mut app, 60, 20);
        app.on_terminal(Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
        let before = screen(&mut app, 60, 20);

        let reply = app.turns.last_mut().unwrap();
        reply.text.push_str("\n\nmore\n\nand more");
        reply.cache = None;
        assert_eq!(before, screen(&mut app, 60, 20));
    }

    #[tokio::test]
    async fn the_chat_title_is_only_for_the_terminal_tab() {
        let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
        type_text(&mut app, "Plan my trip");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;
        app.title = Some("Generated name".into());
        assert_eq!(app.window_title(), "* Generated name");
        assert!(!screen(&mut app, 60, 20).contains("Generated name"));
    }

    #[tokio::test]
    async fn narrow_terminals_do_not_panic() {
        let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
        type_text(&mut app, "hi");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;
        for (width, height) in [(1, 1), (5, 3), (12, 6), (20, 8)] {
            screen(&mut app, width, height);
        }
    }

    #[tokio::test]
    async fn attached_files_are_shown_but_their_contents_are_not() {
        let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
        app.fragments = vec!["HIDDENFILE".into()];
        let before = screen(&mut app, 80, 24);
        assert!(before.contains("1 file attached"), "{before}");

        type_text(&mut app, "hi");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;
        let after = screen(&mut app, 80, 24);
        assert!(after.contains("▍ 1 file attached"), "{after}");
        assert!(!after.contains("HIDDENFILE"), "{after}");
    }

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn prefilling(app: &mut App, cache: u64) {
        type_text(app, "hi");
        enter(app);
        app.prompt_progress =
            Some(PromptProgress { processed: 83_000, total: 197_000, cache, time_ms: 40 });
    }

    #[tokio::test]
    async fn activity_line_shows_prefill_progress_instead_of_connecting() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        prefilling(&mut app, 0);
        let line = text(&activity_line(&app));
        assert!(line.contains("Prefilling 42% (83k/197k)"), "{line}");
        assert!(!line.contains("cached"), "{line}");
        assert!(!line.contains("Connecting"), "{line}");
        assert!(line.contains("esc to stop"), "{line}");
    }

    #[tokio::test]
    async fn activity_line_shows_cached_tokens() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        prefilling(&mut app, 12_000);
        let line = text(&activity_line(&app));
        assert!(line.contains("Prefilling 42% (83k/197k), 12k cached"), "{line}");
    }

    #[tokio::test]
    async fn activity_line_shows_prefill_progress_without_animations() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.animations = false;
        prefilling(&mut app, 0);
        assert!(text(&activity_line(&app)).contains("Prefilling 42%"));
    }

    #[tokio::test]
    async fn status_line_shows_the_system_prompt_size() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.system = Some("x".repeat(813 * 1024));
        let line = text(&status_line(&app, 100));
        assert!(line.contains("sys ~208k tok (813 KB)"), "{line}");
        assert!(!line.contains("custom"), "{line}");
    }

    #[tokio::test]
    async fn status_line_names_the_persona_beside_its_size() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.persona = Some("pirate".into());
        app.system = Some("x".repeat(2048));
        let line = text(&status_line(&app, 100));
        assert!(line.contains("pirate   sys ~512 tok (2 KB)"), "{line}");
    }

    #[tokio::test]
    async fn status_line_shows_fragment_and_prefill_sizes() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.fragments = vec!["a".repeat(1024), "b".repeat(1024)];
        app.prefill = Some("Hello".into());
        let line = text(&status_line(&app, 100));
        assert!(line.contains("2 files attached (2 KB)"), "{line}");
        assert!(line.contains("prefill set (5 B)"), "{line}");
    }

    #[tokio::test]
    async fn status_line_shows_context_usage_when_the_window_is_known() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        assert!(!text(&status_line(&app, 100)).contains('%'));

        app.context_window = Some(258_000);
        prefilling(&mut app, 0);
        let line = text(&status_line(&app, 100));
        assert!(line.trim_end().ends_with("197k/258k (76%)"), "{line}");
    }

    #[tokio::test]
    async fn status_line_marks_estimated_context_usage() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.context_window = Some(1000);
        app.system = Some("x".repeat(400));
        let line = text(&status_line(&app, 100));
        assert!(line.trim_end().ends_with("~100/1k (10%)"), "{line}");
    }

    #[tokio::test]
    async fn status_line_keeps_context_usage_when_totals_do_not_fit() {
        let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
        app.context_window = Some(1000);
        type_text(&mut app, "hi");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;

        let wide = text(&status_line(&app, 80));
        assert!(wide.trim_end().ends_with("8/1k (0%) · 5 in · 3 out"), "{wide}");
        let narrow = text(&status_line(&app, 30));
        assert!(narrow.trim_end().ends_with("8/1k (0%)"), "{narrow}");
    }

    #[tokio::test]
    async fn welcome_screen_stays_plain_without_extras() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        let screen = screen(&mut app, 80, 24);
        assert!(!screen.contains("System prompt"), "{screen}");
        assert!(!screen.contains("Context window"), "{screen}");
    }

    #[tokio::test]
    async fn welcome_screen_lists_what_the_chat_will_send() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.persona = Some("pirate".into());
        app.system = Some("x".repeat(2048));
        app.fragments = vec!["y".repeat(1024)];
        app.prefill = Some("Hello".into());
        app.context_window = Some(258_000);
        let screen = screen(&mut app, 100, 24);
        assert!(screen.contains("System prompt: pirate, ~512 tok (2 KB)"), "{screen}");
        assert!(screen.contains("1 file attached: ~256 tok (1 KB)"), "{screen}");
        assert!(screen.contains("Prefill: ~2 tok (5 B)"), "{screen}");
        assert!(screen.contains("Context window: 258k tokens"), "{screen}");
    }

    /// A finished exchange long enough for a short summary to replace.
    async fn chat_ready_to_compact() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<crate::app::StreamMessage>,
        std::sync::Arc<crate::app::tests::FakeProvider>,
    ) {
        let reply = vec![StreamEvent::Text("a long answer ".repeat(40))];
        let (mut app, mut receiver, provider) = app_with(reply, None);
        type_text(&mut app, "hi");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;
        (app, receiver, provider)
    }

    #[tokio::test]
    async fn activity_line_shows_compaction_and_how_to_stop_it() {
        let (mut app, _receiver, _provider) = chat_ready_to_compact().await;
        type_text(&mut app, "/compact");
        enter(&mut app);
        let line = text(&activity_line(&app));
        assert!(line.contains("Compacting"), "{line}");
        assert!(line.contains("esc to stop"), "{line}");

        app.prompt_progress =
            Some(PromptProgress { processed: 83_000, total: 197_000, cache: 0, time_ms: 40 });
        let line = text(&activity_line(&app));
        assert!(line.contains("Compacting: Prefilling 42%"), "{line}");
    }

    #[tokio::test]
    async fn a_compacted_chat_shows_the_summary_and_what_was_saved() {
        let (mut app, mut receiver, provider) = chat_ready_to_compact().await;
        app.context_window = Some(1000);
        let before = text(&status_line(&app, 100));
        provider.queue(vec![StreamEvent::Text("The **short** version".into())]);
        type_text(&mut app, "/compact");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;

        let shown = screen(&mut app, 80, 24);
        assert!(shown.contains("Summary of earlier conversation"), "{shown}");
        assert!(shown.contains("The short version"), "{shown}");
        assert!(shown.contains("Compacted 2 messages (~141 -> ~24 tokens)"), "{shown}");
        assert!(!shown.contains("a long answer"), "{shown}");
        assert!(!shown.contains("[Summary of the earlier"), "the header is for the model: {shown}");

        let after = text(&status_line(&app, 100));
        assert_ne!(before, after);
        assert!(after.contains("~24/1k (2%)"), "{after}");
    }

    fn open_browser(app: &mut App) {
        type_text(app, "/prompts");
        enter(app);
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_terminal(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn filter(app: &mut App, text: &str) {
        text.chars().for_each(|character| key(app, KeyCode::Char(character)));
    }

    #[tokio::test]
    async fn the_browser_shows_vendors_sizes_and_a_preview() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        open_browser(&mut app);
        filter(&mut app, "gpt-3.5");
        let shown = screen(&mut app, 130, 30);
        for expected in ["System prompts", "OpenAI", "Anthropic", "openai/gpt-3.5", "You are ChatGPT"] {
            assert!(shown.contains(expected), "{expected}: {shown}");
        }
        assert!(shown.contains("context size unknown"), "{shown}");
    }

    #[tokio::test]
    async fn the_browser_marks_how_each_prompt_fits_the_model() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.context_window = Some(8000);
        open_browser(&mut app);
        filter(&mut app, "grok-4.7");
        let over = screen(&mut app, 130, 30);
        assert!(over.contains("% over"), "{over}");
        assert!(over.contains("Over the context limit"), "{over}");

        app.picker.as_mut().unwrap().filter = "claude-haiku-5.5".into();
        let near = screen(&mut app, 130, 30);
        assert!(near.contains("% near"), "{near}");
        assert!(near.contains("Close to the context limit"), "{near}");
    }

    #[tokio::test]
    async fn the_fit_of_a_prompt_counts_the_conversation_so_far() {
        let (mut app, mut receiver, _provider) =
            app_with(vec![StreamEvent::Text("x".repeat(30_000))], None);
        type_text(&mut app, "hi");
        enter(&mut app);
        drain(&mut app, &mut receiver).await;
        let alone = estimate_tokens(4000);
        assert!(app.tokens_with_prompt(4000) > alone + 7000);
    }

    #[tokio::test]
    async fn a_large_system_prompt_keeps_warning_after_the_browser_closes() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.context_window = Some(1000);
        app.system = Some("x".repeat(8000));
        assert!(text(&activity_line(&app)).contains("Over the context limit: ~2k of 1k tokens"));
        app.system = Some("x".repeat(3400));
        assert!(text(&activity_line(&app)).contains("Close to the context limit"));
        app.system = Some("x".repeat(40));
        assert_eq!(text(&activity_line(&app)), "");
    }

    #[tokio::test]
    async fn the_warning_waits_for_the_context_size_to_be_known() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.system = Some("x".repeat(80_000));
        assert_eq!(text(&activity_line(&app)), "");
    }

    #[tokio::test]
    async fn the_browser_fits_small_terminals() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.context_window = Some(8000);
        open_browser(&mut app);
        for (width, height) in [(1, 1), (10, 5), (24, 9), (30, 10), (60, 12), (91, 20), (300, 90)] {
            screen(&mut app, width, height);
        }
        filter(&mut app, "no prompt has this name");
        assert!(screen(&mut app, 80, 24).contains("No prompts match"));
    }

    #[tokio::test]
    async fn typing_a_slash_suggests_commands() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "/e");
        let screen = screen(&mut app, 80, 24);
        assert!(screen.contains("/effort  /edit"), "{screen}");
    }
}

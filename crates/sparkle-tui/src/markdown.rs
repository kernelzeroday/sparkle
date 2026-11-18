//! Markdown to styled terminal lines. The whole source is re-rendered on change, so partial
//! streaming input and terminal resizes need no incremental state.

use std::sync::OnceLock;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::{Icons, Theme};

fn syntax_assets() -> &'static (SyntaxSet, ThemeSet) {
    static ASSETS: OnceLock<(SyntaxSet, ThemeSet)> = OnceLock::new();
    ASSETS.get_or_init(|| (SyntaxSet::load_defaults_newlines(), ThemeSet::load_defaults()))
}

fn push_piece(line: &mut Vec<Span<'static>>, text: &str, style: Style) {
    match line.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(text),
        _ => line.push(Span::styled(text.to_string(), style)),
    }
}

fn finish_line(lines: &mut Vec<Vec<Span<'static>>>, line: &mut Vec<Span<'static>>) {
    while let Some(last) = line.last_mut() {
        let trimmed = last.content.trim_end().len();
        if trimmed == 0 {
            line.pop();
        } else {
            last.content.to_mut().truncate(trimmed);
            break;
        }
    }
    lines.push(std::mem::take(line));
}

/// Splits text into alternating runs of whitespace and non-whitespace.
fn pieces(text: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut previous: Option<bool> = None;
    for (index, character) in text.char_indices() {
        let space = character.is_whitespace();
        if previous.is_some_and(|value| value != space) {
            result.push(&text[start..index]);
            start = index;
        }
        previous = Some(space);
    }
    if start < text.len() {
        result.push(&text[start..]);
    }
    result
}

/// Word-wraps styled spans, breaking words only when one is wider than the line.
pub fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = Vec::new();
    let mut used = 0;
    for span in spans {
        for piece in pieces(&span.content) {
            let piece_width = piece.width();
            if piece.chars().all(char::is_whitespace) {
                if used == 0 {
                    continue;
                }
                if used + piece_width >= width {
                    finish_line(&mut lines, &mut line);
                    used = 0;
                } else {
                    push_piece(&mut line, piece, span.style);
                    used += piece_width;
                }
                continue;
            }
            if used > 0 && used + piece_width > width {
                finish_line(&mut lines, &mut line);
                used = 0;
            }
            if piece_width <= width {
                push_piece(&mut line, piece, span.style);
                used += piece_width;
                continue;
            }
            for character in piece.chars() {
                let character_width = character.width().unwrap_or(0);
                if used + character_width > width {
                    finish_line(&mut lines, &mut line);
                    used = 0;
                }
                push_piece(&mut line, character.encode_utf8(&mut [0; 4]), span.style);
                used += character_width;
            }
        }
    }
    if !line.is_empty() {
        finish_line(&mut lines, &mut line);
    }
    lines
}

/// True when the source ends inside a code fence that has not been closed yet.
fn ends_in_open_fence(source: &str) -> bool {
    let mut open: Option<(char, usize)> = None;
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some(marker) = trimmed.chars().next().filter(|c| *c == '`' || *c == '~') else {
            continue;
        };
        let count = trimmed.chars().take_while(|c| *c == marker).count();
        if count < 3 {
            continue;
        }
        match open {
            None => open = Some((marker, count)),
            Some((fence, size))
                if fence == marker && count >= size && trimmed[count..].trim().is_empty() =>
            {
                open = None;
            }
            Some(_) => {}
        }
    }
    open.is_some()
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
}

struct Writer<'a> {
    theme: &'a Theme,
    icons: &'a Icons,
    width: usize,
    out: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    indents: Vec<(String, Style)>,
    marker: Option<String>,
    lists: Vec<Option<u64>>,
    code: Option<(String, String)>,
    table: Option<Table>,
    links: Vec<String>,
    gap: bool,
}

impl Writer<'_> {
    fn style(&self) -> Style {
        *self.styles.last().expect("the style stack is never empty")
    }

    fn push_style(&mut self, change: impl FnOnce(Style) -> Style) {
        self.styles.push(change(self.style()));
    }

    fn text(&mut self, text: &str) {
        if let Some(table) = &mut self.table {
            table.cell.push_str(text);
        } else if let Some((_, code)) = &mut self.code {
            code.push_str(text);
        } else {
            let style = self.style();
            self.spans.push(Span::styled(text.to_string(), style));
        }
    }

    fn start_block(&mut self) {
        self.flush();
        if self.gap && !self.out.is_empty() {
            self.out.push(Line::default());
        }
        self.gap = false;
    }

    fn prefix_width(&self) -> usize {
        self.indents.iter().map(|(text, _)| text.width()).sum()
    }

    fn prefix(&mut self, first: bool) -> Vec<Span<'static>> {
        let last = self.indents.len().saturating_sub(1);
        let marker = if first { self.marker.take() } else { None };
        let mut spans = Vec::new();
        for (index, (text, style)) in self.indents.iter().enumerate() {
            match &marker {
                Some(marker) if index == last => {
                    spans.push(Span::styled(marker.clone(), Style::new().fg(self.theme.accent)));
                }
                _ => spans.push(Span::styled(text.clone(), *style)),
            }
        }
        spans
    }

    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let available = self.width.saturating_sub(self.prefix_width()).max(8);
        for (index, wrapped) in wrap_spans(&spans, available).into_iter().enumerate() {
            let mut line = self.prefix(index == 0);
            line.extend(wrapped);
            self.out.push(Line::from(line));
        }
    }

    fn emit_code(&mut self, language: &str, code: &str, highlight: bool) {
        let available = self.width.saturating_sub(self.prefix_width()).max(8);
        let background = Style::new().bg(self.theme.code_bg);
        let label = if language.is_empty() { "text" } else { language };
        let header = format!(" {} {label}", self.icons.code);
        let padding = " ".repeat(available.saturating_sub(header.width()));
        let mut line = self.prefix(false);
        line.push(Span::styled(header, background.fg(self.theme.muted)));
        line.push(Span::styled(padding, background));
        self.out.push(Line::from(line));

        let (syntaxes, themes) = syntax_assets();
        let syntax = syntaxes
            .find_syntax_by_token(language)
            .filter(|_| highlight && !language.is_empty());
        let mut highlighter =
            syntax.map(|syntax| HighlightLines::new(syntax, &themes.themes[self.theme.syntax_theme]));
        let content_width = available.saturating_sub(2).max(1);
        for source in LinesWithEndings::from(code) {
            let segments: Vec<(Style, String)> = highlighter
                .as_mut()
                .and_then(|highlighter| highlighter.highlight_line(source, syntaxes).ok())
                .map_or_else(
                    || vec![(background.fg(self.theme.text), source.to_string())],
                    |ranges| {
                        ranges
                            .into_iter()
                            .map(|(style, text)| {
                                let color = style.foreground;
                                (background.fg(Color::Rgb(color.r, color.g, color.b)), text.to_string())
                            })
                            .collect()
                    },
                );
            let mut rows: Vec<(Vec<Span<'static>>, usize)> = vec![(Vec::new(), 0)];
            for (style, text) in segments {
                let text = text.trim_end_matches(['\n', '\r']).replace('\t', "    ");
                for character in text.chars() {
                    let width = character.width().unwrap_or(0);
                    if rows.last().expect("rows is never empty").1 + width > content_width {
                        rows.push((Vec::new(), 0));
                    }
                    let row = rows.last_mut().expect("rows is never empty");
                    push_piece(&mut row.0, character.encode_utf8(&mut [0; 4]), style);
                    row.1 += width;
                }
            }
            for (spans, used) in rows {
                let mut line = self.prefix(false);
                line.push(Span::styled("  ", background));
                line.extend(spans);
                line.push(Span::styled(" ".repeat(content_width - used), background));
                self.out.push(Line::from(line));
            }
        }
    }

    fn emit_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![0; columns];
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.width());
            }
        }
        let available = self.width.saturating_sub(self.prefix_width() + 3 * (columns - 1)).max(columns);
        while widths.iter().sum::<usize>() > available {
            let widest = (0..columns).max_by_key(|index| widths[*index]).expect("columns > 0");
            if widths[widest] <= 1 {
                break;
            }
            widths[widest] -= 1;
        }
        let border = Style::new().fg(self.theme.faint);
        for (row_index, row) in table.rows.iter().enumerate() {
            let cell_style = if row_index == 0 {
                Style::new().fg(self.theme.heading).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(self.theme.text)
            };
            let mut line = self.prefix(false);
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    line.push(Span::styled(" │ ", border));
                }
                let cell = row.get(index).map_or("", String::as_str);
                line.push(Span::styled(fit(cell, *width), cell_style));
            }
            self.out.push(Line::from(line));
            if row_index == 0 {
                let rule: Vec<String> = widths.iter().map(|width| "─".repeat(*width)).collect();
                let mut line = self.prefix(false);
                line.push(Span::styled(rule.join("─┼─"), border));
                self.out.push(Line::from(line));
            }
        }
    }
}

/// Pads or truncates a table cell to an exact display width.
fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return format!("{text}{}", " ".repeat(width - text.width()));
    }
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width + 1 > width {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    format!("{result}{}", " ".repeat(width.saturating_sub(used + 1)))
}

pub fn render(source: &str, width: u16, theme: &Theme, icons: &Icons) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let open_fence = ends_in_open_fence(source);
    let mut writer = Writer {
        theme,
        icons,
        width: usize::from(width),
        out: Vec::new(),
        spans: Vec::new(),
        styles: vec![Style::new().fg(theme.text)],
        indents: Vec::new(),
        marker: None,
        lists: Vec::new(),
        code: None,
        table: None,
        links: Vec::new(),
        gap: false,
    };

    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => writer.start_block(),
                Tag::Heading { level, .. } => {
                    writer.start_block();
                    let heading = theme.heading;
                    writer.push_style(|style| style.fg(heading).add_modifier(Modifier::BOLD));
                    if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                        writer.text("▍ ");
                    }
                }
                Tag::BlockQuote(_) => {
                    writer.start_block();
                    writer.indents.push(("│ ".into(), Style::new().fg(theme.faint)));
                    let muted = theme.muted;
                    writer.push_style(|style| style.fg(muted).add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    writer.start_block();
                    let language = match kind {
                        CodeBlockKind::Fenced(info) => {
                            info.split_whitespace().next().unwrap_or("").to_string()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    writer.code = Some((language, String::new()));
                }
                Tag::List(start) => {
                    if writer.lists.is_empty() {
                        writer.start_block();
                    } else {
                        writer.flush();
                    }
                    writer.lists.push(start);
                }
                Tag::Item => {
                    writer.start_block();
                    let marker = match writer.lists.last_mut() {
                        Some(Some(number)) => {
                            let marker = format!("{number}. ");
                            *number += 1;
                            marker
                        }
                        _ => format!("{} ", icons.bullet),
                    };
                    writer.indents.push((" ".repeat(marker.width()), Style::new()));
                    writer.marker = Some(marker);
                }
                Tag::Table(_) => {
                    writer.start_block();
                    writer.table = Some(Table::default());
                }
                Tag::Emphasis => writer.push_style(|style| style.add_modifier(Modifier::ITALIC)),
                Tag::Strong => writer.push_style(|style| style.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => {
                    writer.push_style(|style| style.add_modifier(Modifier::CROSSED_OUT));
                }
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    writer.links.push(dest_url.to_string());
                    let link = theme.link;
                    writer.push_style(|style| style.fg(link).add_modifier(Modifier::UNDERLINED));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    writer.flush();
                    writer.gap = true;
                }
                TagEnd::Heading(_) => {
                    writer.flush();
                    writer.styles.pop();
                    writer.gap = true;
                }
                TagEnd::BlockQuote(_) => {
                    writer.flush();
                    writer.indents.pop();
                    writer.styles.pop();
                    writer.gap = true;
                }
                TagEnd::CodeBlock => {
                    if let Some((language, code)) = writer.code.take() {
                        let unfinished = open_fence && range.end >= source.trim_end().len();
                        writer.emit_code(&language, &code, !unfinished);
                    }
                    writer.gap = true;
                }
                TagEnd::List(_) => {
                    writer.flush();
                    writer.lists.pop();
                    writer.gap = writer.lists.is_empty();
                }
                TagEnd::Item => {
                    writer.flush();
                    writer.indents.pop();
                    writer.marker = None;
                }
                TagEnd::TableCell => {
                    if let Some(table) = &mut writer.table {
                        let cell = std::mem::take(&mut table.cell);
                        table.row.push(cell);
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(table) = &mut writer.table {
                        let row = std::mem::take(&mut table.row);
                        table.rows.push(row);
                    }
                }
                TagEnd::Table => {
                    if let Some(table) = writer.table.take() {
                        writer.emit_table(table);
                    }
                    writer.gap = true;
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    writer.styles.pop();
                }
                TagEnd::Link | TagEnd::Image => {
                    writer.styles.pop();
                    if let Some(url) = writer.links.pop() {
                        let already_shown =
                            writer.spans.last().is_some_and(|span| span.content == url.as_str());
                        if !already_shown {
                            writer.spans.push(Span::styled(
                                format!(" ({url})"),
                                Style::new().fg(theme.muted),
                            ));
                        }
                    }
                }
                _ => {}
            },
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => writer.text(&text),
            Event::Code(text) => {
                if let Some(table) = &mut writer.table {
                    table.cell.push_str(&text);
                } else {
                    writer.spans.push(Span::styled(
                        text.to_string(),
                        Style::new().fg(theme.inline_code).bg(theme.code_bg),
                    ));
                }
            }
            Event::SoftBreak => writer.text(" "),
            Event::HardBreak => writer.flush(),
            Event::Rule => {
                writer.start_block();
                let rule = "─".repeat(writer.width.saturating_sub(writer.prefix_width()));
                let mut line = writer.prefix(false);
                line.push(Span::styled(rule, Style::new().fg(theme.faint)));
                writer.out.push(Line::from(line));
                writer.gap = true;
            }
            Event::TaskListMarker(done) => writer.text(if done { "☑ " } else { "☐ " }),
            _ => {}
        }
    }
    writer.flush();
    writer.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(source: &str, width: u16) -> Vec<String> {
        render(source, width, &Theme::DUSK, &Icons::ASCII)
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    #[test]
    fn wraps_words_without_exceeding_width() {
        let lines = plain("alpha beta gamma delta", 11);
        assert_eq!(lines, ["alpha beta", "gamma delta"]);
    }

    #[test]
    fn breaks_words_wider_than_the_line() {
        assert_eq!(plain("abcdefghijkl", 8), ["abcdefgh", "ijkl"]);
    }

    #[test]
    fn wide_characters_respect_display_width() {
        for line in plain("日本語のテキストを折り返します", 10) {
            assert!(line.width() <= 10, "{line}");
        }
    }

    #[test]
    fn renders_lists_with_hanging_indent() {
        let lines = plain("- one two three four\n- next\n\n1. first\n2. second", 12);
        assert_eq!(lines, ["- one two", "  three four", "- next", "", "1. first", "2. second"]);
    }

    #[test]
    fn renders_nested_lists() {
        assert_eq!(plain("- a\n  - b\n- c", 20), ["- a", "  - b", "- c"]);
    }

    #[test]
    fn renders_code_block_with_language_header() {
        let lines = plain("```rust\nfn main() {}\n```", 30);
        assert_eq!(lines, [" # rust", "  fn main() {}"]);
    }

    #[test]
    fn unfinished_code_block_renders_while_streaming() {
        let lines = plain("text\n\n```python\nprint(", 30);
        assert_eq!(lines, ["text", "", " # python", "  print("]);
    }

    #[test]
    fn code_lines_are_padded_to_a_solid_panel() {
        let lines = render("```\nx\n```", 20, &Theme::DUSK, &Icons::ASCII);
        for line in &lines {
            assert_eq!(line.width(), 20);
        }
    }

    #[test]
    fn renders_tables_with_aligned_columns() {
        let lines = plain("| a | bb |\n|---|---|\n| ccc | d |", 30);
        assert_eq!(lines, ["a   │ bb", "────┼───", "ccc │ d"]);
    }

    #[test]
    fn narrow_tables_truncate_cells() {
        let lines = plain("| long heading | other |\n|---|---|\n| value | x |", 14);
        assert!(lines.iter().all(|line| line.width() <= 14), "{lines:?}");
        assert!(lines[0].contains('…'));
    }

    #[test]
    fn links_show_their_destination() {
        assert_eq!(plain("[site](https://a.io)", 40), ["site (https://a.io)"]);
        assert_eq!(plain("<https://a.io>", 40), ["https://a.io"]);
    }

    fn styled(source: &str) -> Vec<Span<'static>> {
        render(source, 40, &Theme::DUSK, &Icons::ASCII).remove(0).spans
    }

    fn modifier_of(spans: &[Span<'static>], text: &str) -> Modifier {
        spans.iter().find(|span| span.content == text).expect("the text is present").style.add_modifier
    }

    #[test]
    fn renders_headings_by_level() {
        assert_eq!(plain("# Title\n\ntext", 20), ["▍ Title", "", "text"]);
        assert_eq!(plain("## Part", 20), ["▍ Part"]);
        assert_eq!(plain("### Small", 20), ["Small"]);
    }

    #[test]
    fn renders_quotes_rules_and_breaks() {
        assert_eq!(plain("> quoted words", 20), ["│ quoted words"]);
        assert_eq!(plain("a\n\n---\n\nb", 5), ["a", "", "─────", "", "b"]);
        assert_eq!(plain("line one  \nline two", 20), ["line one", "line two"]);
        assert_eq!(plain("soft\nbreak", 20), ["soft break"]);
    }

    #[test]
    fn renders_task_lists_images_and_html() {
        assert_eq!(plain("- [x] done\n- [ ] todo", 20), ["- ☑ done", "- ☐ todo"]);
        assert_eq!(plain("![alt](http://a.io/i.png)", 40), ["alt (http://a.io/i.png)"]);
        assert_eq!(plain("a <b>bold</b> c", 40), ["a <b>bold</b> c"]);
    }

    #[test]
    fn applies_inline_styles() {
        let spans = styled("**b** *i* ~~s~~ `c`");
        assert!(modifier_of(&spans, "b").contains(Modifier::BOLD));
        assert!(modifier_of(&spans, "i").contains(Modifier::ITALIC));
        assert!(modifier_of(&spans, "s").contains(Modifier::CROSSED_OUT));
        let code = spans.iter().find(|span| span.content == "c").unwrap();
        assert_eq!(code.style.bg, Some(Theme::DUSK.code_bg));
    }

    #[test]
    fn renders_every_kind_of_code_block() {
        assert_eq!(plain("    indented", 20), [" # text", "  indented"]);
        assert_eq!(plain("~~~py\nx\n~~~", 20), [" # py", "  x"]);
        assert_eq!(plain("```\n\tx\n```", 20), [" # text", "      x"]);
        assert_eq!(plain("```\nabcdefghijkl\n```", 8), [" # text", "  abcdef", "  ghijkl"]);
    }

    #[test]
    fn closed_code_is_highlighted_and_open_code_is_not() {
        let spans = |source: &str| {
            let lines = render(source, 40, &Theme::DUSK, &Icons::ASCII);
            lines[1].spans.len()
        };
        assert!(spans("```rust\nfn main() {}\n```") > 3);
        assert_eq!(spans("```rust\nfn main() {}"), 3);
    }

    #[test]
    fn table_cells_keep_inline_code_text() {
        assert_eq!(plain("| `a` |\n|---|\n| b |", 20), ["a", "─", "b"]);
    }

    #[test]
    fn detects_open_fences() {
        assert!(ends_in_open_fence("```rust\nfn"));
        assert!(!ends_in_open_fence("```rust\nfn\n```\n"));
        assert!(ends_in_open_fence("````\n```\n"));
    }
}

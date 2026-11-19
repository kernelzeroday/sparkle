//! Selecting text on screen with the mouse, the way a terminal would: from where the button went
//! down to where it is now, in reading order. Works on the text as drawn, one string per cell, so
//! what is copied is exactly what was highlighted.

/// A cell on screen, as (column, row).
pub type Point = (u16, u16);

/// The cells' text, row by row. A wide character fills its first cell; the cells it covers after
/// that are empty strings.
pub type Screen = Vec<Vec<String>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
}

impl Selection {
    pub fn new(at: Point) -> Self {
        Self { anchor: at, head: at }
    }

    /// The two ends in reading order.
    fn ordered(&self) -> (Point, Point) {
        let key = |(column, row): Point| (row, column);
        if key(self.anchor) <= key(self.head) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// True when the selection covers more than the single cell that was clicked.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn contains(&self, (column, row): Point) -> bool {
        let ((first_column, first_row), (last_column, last_row)) = self.ordered();
        if row < first_row || row > last_row {
            return false;
        }
        (row != first_row || column >= first_column) && (row != last_row || column <= last_column)
    }

    /// The selected text: each row's selected cells, without trailing spaces, joined by newlines.
    pub fn text(&self, screen: &Screen) -> String {
        let ((first_column, first_row), (last_column, last_row)) = self.ordered();
        let mut lines = Vec::new();
        for row in first_row..=last_row {
            let Some(cells) = screen.get(usize::from(row)) else { break };
            let start = if row == first_row { usize::from(first_column) } else { 0 };
            let end = if row == last_row { usize::from(last_column) + 1 } else { cells.len() };
            let line: String = cells.iter().take(end).skip(start).map(String::as_str).collect();
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// The word under a cell, for a double click: the run of non-blank cells around it.
    pub fn word(screen: &Screen, (column, row): Point) -> Option<Self> {
        let cells = screen.get(usize::from(row))?;
        let blank = |index: usize| cells.get(index).is_none_or(|cell| cell.trim().is_empty() && !cell.is_empty());
        let at = usize::from(column);
        if blank(at) {
            return None;
        }
        let mut start = at;
        while start > 0 && !blank(start - 1) {
            start -= 1;
        }
        let mut end = at;
        while end + 1 < cells.len() && !blank(end + 1) {
            end += 1;
        }
        Some(Self { anchor: (start as u16, row), head: (end as u16, row) })
    }

    /// The whole line under a cell, for a triple click, up to its last non-blank cell.
    pub fn line(screen: &Screen, row: u16) -> Option<Self> {
        let cells = screen.get(usize::from(row))?;
        let last = cells.iter().rposition(|cell| !cell.trim().is_empty())?;
        let first = cells.iter().position(|cell| !cell.trim().is_empty())?;
        Some(Self { anchor: (first as u16, row), head: (last as u16, row) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: &[&str]) -> Screen {
        rows.iter().map(|row| row.chars().map(|c| c.to_string()).collect()).collect()
    }

    #[test]
    fn a_drag_selects_in_reading_order_whichever_way_it_goes() {
        let text = screen(&["hello world   ", "second line   ", "third         "]);
        let forward = Selection { anchor: (6, 0), head: (5, 1) };
        assert_eq!(forward.text(&text), "world\nsecond");
        let backward = Selection { anchor: (5, 1), head: (6, 0) };
        assert_eq!(backward.text(&text), "world\nsecond");
        let across = Selection { anchor: (0, 0), head: (13, 2) };
        assert_eq!(across.text(&text), "hello world\nsecond line\nthird");
        assert!(forward.contains((10, 0)) && forward.contains((0, 1)) && !forward.contains((6, 1)));
        assert!(!forward.contains((5, 0)) && !forward.contains((0, 2)));
    }

    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        assert!(Selection::new((3, 1)).is_empty());
        assert!(!Selection { anchor: (3, 1), head: (4, 1) }.is_empty());
    }

    #[test]
    fn double_and_triple_clicks_pick_a_word_and_a_line() {
        let text = screen(&["  let value = 42;  "]);
        let word = Selection::word(&text, (7, 0)).unwrap();
        assert_eq!(word.text(&text), "value");
        assert_eq!(Selection::word(&text, (0, 0)), None, "a blank cell has no word");
        let line = Selection::line(&text, 0).unwrap();
        assert_eq!(line.text(&text), "let value = 42;");
    }

    #[test]
    fn wide_characters_are_copied_once() {
        // A wide character's second cell is an empty string.
        let text: Screen = vec![vec!["日".into(), String::new(), "本".into(), String::new(), " ".into()]];
        assert_eq!(Selection { anchor: (0, 0), head: (3, 0) }.text(&text), "日本");
        assert_eq!(Selection::word(&text, (1, 0)).unwrap().text(&text), "日本");
    }
}

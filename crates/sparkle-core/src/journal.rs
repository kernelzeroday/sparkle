//! The chat model's journal: short notes she chooses to keep about the person she talks with.
//!
//! She writes inline, anywhere in a reply, so keeping a note costs no extra request:
//!
//! ```text
//! <journal keep="3w">Learning Rust for a new job; likes worked examples.</journal>
//! <journal forget="new job"/>
//! ```
//!
//! The tags stay in the conversation she sees, so she remembers having written them, but are
//! hidden from the transcript. Each note fades when the time she gave it runs out. The notes
//! still in force are shown to her once, with the system prompt, when a conversation opens.
//! They are never refreshed mid-conversation, which keeps local prompt caches valid and keeps
//! the journal in the background rather than in front of every turn.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const TAG: &str = "<journal";
const CLOSE: &str = "</journal>";
/// A note longer than this is cut, so that a single note cannot crowd out the rest.
const MAX_NOTE_CHARS: usize = 400;
/// How long a note lasts when she does not say.
const DEFAULT_KEEP_SECS: u64 = 30 * DAY;
/// Room for notes in the opening context, in bytes (about 750 tokens).
const BLOCK_BUDGET: usize = 3000;

const HOUR: u64 = 60 * 60;
const DAY: u64 = 24 * HOUR;

/// How a conversation opens when the journal is on. Kept short and deliberately unforceful:
/// writing is optional, and the notes are background rather than an agenda.
const GUIDE: &str = "\
You keep a journal across your conversations with this person: a place for things you would \
want to remember about them, like what they care about, how they like to be helped, or threads \
worth returning to someday. Writing in it is entirely your choice, and most replies need \
nothing. To keep a note, put it anywhere in a reply:
<journal keep=\"3w\">one short note</journal>
keep is how long the note stays with you: days, weeks, months or years (5d, 3w, 6mo, 1y), or \
always. Notes fade when their time is up, and writing the same note again renews it. To let \
a note go early, quote a few of its words: <journal forget=\"words from the note\"/>. A tag only \
takes effect when it is written in your reply itself: planning one in your thinking, or saying \
you kept or let go of a note, does not do it. Journal tags are hidden from the conversation, \
though the person can open the journal if they choose to.";

const NOTES_INTRO: &str = "\
Your notes so far. They are background only: let them inform you quietly, with no need to \
mention them, act on them, or tend to them.";

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_secs())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub text: String,
    /// Unix seconds when the note was written or last renewed.
    pub written: u64,
    /// Unix seconds when the note fades, or `None` to keep it always.
    pub until: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Entry {
    fn is_live(&self, at: u64) -> bool {
        self.until.is_none_or(|until| until > at)
    }
}

/// Something she wrote in a reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// A note to keep, and for how many seconds (`None` for always).
    Keep { text: String, keep: Option<u64> },
    /// Lets go of the one note whose text contains these words. Words rather than numbers,
    /// because a note written earlier in the same conversation has no place in the list she was
    /// shown, and a guessed number would let go of the wrong note.
    Forget(String),
}

/// What carrying out a reply's actions changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub kept: usize,
    pub let_go: usize,
}

impl Applied {
    pub fn any(self) -> bool {
        self.kept + self.let_go > 0
    }
}

fn forget_action(value: &str) -> Option<Action> {
    let value = value.trim();
    // A bare number does nothing: notes are not numbered, so it can only be a guess.
    let usable = !value.is_empty() && !value.chars().all(|c| c.is_ascii_digit());
    usable.then(|| Action::Forget(value.to_string()))
}

/// The words of a note, lowercased and without punctuation, so that a quote she shortens or
/// rewords a little ("back soon test journal") still finds its note.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Reads a duration such as `5d`, `3w`, `6m`, `1y`, `12h` or `always`.
pub fn parse_keep(value: &str) -> Option<Option<u64>> {
    let value = value.trim().to_ascii_lowercase();
    if matches!(value.as_str(), "always" | "forever" | "permanent") {
        return Some(None);
    }
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = value.split_at(split);
    let number: u64 = number.parse().ok().filter(|number| *number > 0)?;
    let unit = match unit.trim() {
        "h" | "hour" | "hours" => HOUR,
        "d" | "day" | "days" => DAY,
        "w" | "week" | "weeks" => 7 * DAY,
        "m" | "mo" | "month" | "months" => 30 * DAY,
        "y" | "year" | "years" => 365 * DAY,
        _ => return None,
    };
    Some(Some(number.saturating_mul(unit)))
}

/// Says roughly how long a span is, in the units she writes durations in.
fn span(seconds: u64) -> String {
    match seconds {
        s if s < DAY => format!("{}h", (s / HOUR).max(1)),
        s if s < 14 * DAY => format!("{}d", s / DAY),
        s if s < 60 * DAY => format!("{}w", s / (7 * DAY)),
        // "mo", not "m", which reads as minutes.
        s if s < 365 * DAY => format!("{}mo", s / (30 * DAY)),
        s => format!("{}y", s / (365 * DAY)),
    }
}

/// The value of `name="..."` (or single quotes) among a tag's attributes.
fn attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attributes;
    while let Some(index) = rest.find(name) {
        let after = rest[index + name.len()..].trim_start();
        let before_ok = index == 0 || !rest.as_bytes()[index - 1].is_ascii_alphanumeric();
        if before_ok && let Some(value) = after.strip_prefix('=') {
            let value = value.trim_start();
            let quote = value.chars().next()?;
            if quote == '"' || quote == '\'' {
                let value = &value[1..];
                return value.find(quote).map(|end| &value[..end]);
            }
            let end = value.find(|c: char| c.is_whitespace() || c == '/' || c == '>');
            return Some(&value[..end.unwrap_or(value.len())]);
        }
        rest = &rest[index + name.len()..];
    }
    None
}

/// A tag found in text: where it starts and ends, and what it asks for.
struct Found {
    start: usize,
    end: usize,
    action: Option<Action>,
}

/// Finds the next complete journal tag at or after `from`, or where an unfinished one starts.
fn next_tag(text: &str, from: usize) -> Option<Result<Found, usize>> {
    let start = from + text[from..].find(TAG)?;
    let after_name = start + TAG.len();
    // `<journalism>` is not a tag.
    if text[after_name..].chars().next().is_some_and(|c| !(c.is_whitespace() || c == '>' || c == '/'))
    {
        return next_tag(text, after_name);
    }
    let Some(open_end) = text[after_name..].find('>').map(|index| after_name + index) else {
        return Some(Err(start));
    };
    let attributes = &text[after_name..open_end];
    if attributes.trim_end().ends_with('/') {
        let action = attribute(attributes, "forget").and_then(forget_action);
        return Some(Ok(Found { start, end: open_end + 1, action }));
    }
    let Some(close) = text[open_end + 1..].find(CLOSE).map(|index| open_end + 1 + index) else {
        return Some(Err(start));
    };
    let body = text[open_end + 1..close].split_whitespace().collect::<Vec<_>>().join(" ");
    let body: String = body.chars().take(MAX_NOTE_CHARS).collect();
    let action = if let Some(value) = attribute(attributes, "forget") {
        forget_action(value)
    } else if body.is_empty() {
        None
    } else {
        let keep = attribute(attributes, "keep").and_then(parse_keep).unwrap_or(Some(DEFAULT_KEEP_SECS));
        Some(Action::Keep { text: body, keep })
    };
    Some(Ok(Found { start, end: close + CLOSE.len(), action }))
}

/// Everything she asked of the journal in a finished reply.
pub fn actions(text: &str) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut from = 0;
    while let Some(Ok(found)) = next_tag(text, from) {
        actions.extend(found.action);
        from = found.end;
    }
    actions
}

/// The reply as the person sees it: tags removed, including one still arriving, and any line
/// left empty by a removed tag dropped.
pub fn strip(text: &str) -> String {
    if !text.contains('<') {
        return text.to_string();
    }
    let mut shown = String::with_capacity(text.len());
    let mut from = 0;
    let mut removed = false;
    loop {
        match next_tag(text, from) {
            Some(Ok(found)) => {
                shown.push_str(&text[from..found.start]);
                from = found.end;
                removed = true;
            }
            Some(Err(start)) => {
                shown.push_str(&text[from..start]);
                from = text.len();
                removed = true;
                break;
            }
            None => break,
        }
    }
    shown.push_str(&text[from..]);
    // While streaming, the start of a tag can arrive before the rest of its name.
    if let Some(index) = shown.rfind('<')
        && TAG.starts_with(&shown[index..])
    {
        shown.truncate(index);
    }
    if !removed {
        return shown;
    }
    let mut tidy = String::with_capacity(shown.len());
    let mut blank_run = 0;
    for line in shown.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        tidy.push_str(line.trim_end());
        tidy.push('\n');
    }
    tidy.trim_matches('\n').to_string()
}

/// The journal on disk, one JSON note per line.
#[derive(Clone, Debug)]
pub struct Journal {
    path: PathBuf,
}

/// What a conversation is shown when it opens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Opening {
    pub text: String,
    /// Ids of the notes shown, oldest first.
    pub ids: Vec<String>,
}

impl Journal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The notes still in force. Faded ones are dropped; an unreadable line is skipped.
    pub fn entries(&self) -> Vec<Entry> {
        self.entries_at(now())
    }

    fn entries_at(&self, at: u64) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Vec::new() };
        text.lines()
            .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
            .filter(|entry| entry.is_live(at))
            .collect()
    }

    fn save(&self, entries: &[Entry]) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = String::new();
        for entry in entries {
            text.push_str(&serde_json::to_string(entry).map_err(std::io::Error::other)?);
            text.push('\n');
        }
        // Written aside and moved into place so that a crash cannot leave half a journal.
        let staging = self.path.with_extension("jsonl.tmp");
        std::fs::write(&staging, text)?;
        std::fs::rename(staging, &self.path)
    }

    /// Carries out what she wrote.
    pub fn apply(&self, actions: &[Action], model: Option<&str>) -> std::io::Result<Applied> {
        self.apply_at(actions, model, now())
    }

    fn apply_at(&self, actions: &[Action], model: Option<&str>, at: u64) -> std::io::Result<Applied> {
        let mut applied = Applied::default();
        if actions.is_empty() {
            return Ok(applied);
        }
        let mut entries = self.entries_at(at);
        let mut changed = 0;
        for action in actions {
            match action {
                Action::Keep { text, keep } => {
                    let until = keep.map(|keep| at.saturating_add(keep));
                    // The same note written again is renewed rather than repeated.
                    let same = |entry: &&mut Entry| entry.text.eq_ignore_ascii_case(text);
                    if let Some(entry) = entries.iter_mut().find(same) {
                        entry.written = at;
                        entry.until = until;
                    } else {
                        let nanos = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_or(0, |time| time.subsec_nanos());
                        entries.push(Entry {
                            id: format!("{at:x}-{nanos:x}-{changed}"),
                            text: text.clone(),
                            written: at,
                            until,
                            model: model.map(str::to_string),
                        });
                    }
                    changed += 1;
                    applied.kept += 1;
                }
                Action::Forget(quoted) => {
                    let wanted = words(quoted);
                    let matches: Vec<usize> = entries
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| {
                            let have = words(&entry.text);
                            !wanted.is_empty() && wanted.iter().all(|word| have.contains(word))
                        })
                        .map(|(index, _)| index)
                        .collect();
                    // Vague words that match several notes let go of none.
                    if let [index] = matches.as_slice() {
                        entries.remove(*index);
                        changed += 1;
                        applied.let_go += 1;
                    }
                }
            }
        }
        if changed > 0 {
            self.save(&entries)?;
        }
        Ok(applied)
    }

    /// Removes every note, for the person who wants the journal cleared.
    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// The guide and notes a conversation opens with.
    pub fn opening(&self) -> Opening {
        opening_from(self.entries(), now())
    }
}

/// Lays out the notes newest-last, keeping the newest that fit the budget.
fn opening_from(mut entries: Vec<Entry>, at: u64) -> Opening {
    entries.sort_by_key(|entry| entry.written);
    let mut kept: Vec<Entry> = Vec::new();
    let mut used = 0;
    for entry in entries.into_iter().rev() {
        let cost = entry.text.len() + 24;
        if used + cost > BLOCK_BUDGET {
            break;
        }
        used += cost;
        kept.push(entry);
    }
    kept.reverse();
    let mut text = GUIDE.to_string();
    if !kept.is_empty() {
        text.push_str("\n\n");
        text.push_str(NOTES_INTRO);
    }
    for entry in &kept {
        let age = span(at.saturating_sub(entry.written));
        let left = entry.until.map_or("kept".to_string(), |until| {
            format!("{} left", span(until.saturating_sub(at)))
        });
        text.push_str(&format!("\n- [{age} ago, {left}] {}", entry.text));
    }
    Opening { text, ids: kept.into_iter().map(|entry| entry.id).collect() }
}

/// One line per note for the person reading the journal.
pub fn describe(entry: &Entry) -> String {
    let at = now();
    let left = entry.until.map_or("always".to_string(), |until| {
        format!("{} left", span(until.saturating_sub(at)))
    });
    format!("{} ago · {left} · {}", span(at.saturating_sub(entry.written)), entry.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_journal(name: &str) -> Journal {
        let path = std::env::temp_dir()
            .join(format!("sparkle-journal-{}-{name}", std::process::id()))
            .join("journal.jsonl");
        let _ = std::fs::remove_file(&path);
        Journal::new(path)
    }

    fn keep(text: &str, keep: Option<u64>) -> Action {
        Action::Keep { text: text.into(), keep }
    }

    #[test]
    fn durations_read_the_way_she_writes_them() {
        assert_eq!(parse_keep("5d"), Some(Some(5 * DAY)));
        assert_eq!(parse_keep("3w"), Some(Some(21 * DAY)));
        assert_eq!(parse_keep("6m"), Some(Some(180 * DAY)));
        assert_eq!(parse_keep("6mo"), Some(Some(180 * DAY)));
        // Shown back as "mo", which cannot be mistaken for minutes.
        assert_eq!(span(364 * DAY), "12mo");
        assert_eq!(span(90 * DAY), "3mo");
        assert_eq!(parse_keep("1y"), Some(Some(365 * DAY)));
        assert_eq!(parse_keep("12 hours"), Some(Some(12 * HOUR)));
        assert_eq!(parse_keep("always"), Some(None));
        assert_eq!(parse_keep("soon"), None);
        assert_eq!(parse_keep("0d"), None);
    }

    #[test]
    fn notes_and_forgetting_are_read_from_a_reply() {
        let reply = "Sure!\n<journal keep=\"2w\">Prefers  short\n answers</journal>\nAlso \
                     <journal>No keep given</journal> and <journal forget=\"cats\"/> done. \
                     <journal keep='always'>Name is Sam</journal> <journalism> stays.";
        assert_eq!(
            actions(reply),
            vec![
                keep("Prefers short answers", Some(14 * DAY)),
                keep("No keep given", Some(DEFAULT_KEEP_SECS)),
                Action::Forget("cats".into()),
                keep("Name is Sam", None),
            ]
        );
    }

    #[test]
    fn a_note_can_be_let_go_by_a_few_of_its_words() {
        // A bare number is only a guess, since notes are not numbered, so it does nothing.
        assert_eq!(
            actions("<journal forget=\"back soon\"/> <journal forget='2'></journal> <journal forget=\"\"/>"),
            vec![Action::Forget("back soon".into())]
        );
        let journal = temp_journal("forget-words");
        let at = 1_000_000;
        let notes = [
            keep("Name is Kelsey", None),
            keep("Kelsey said they'll be back soon to test the journal", None),
            keep("Kelsey likes tea", None),
        ];
        journal.apply_at(&notes, None, at).unwrap();
        // Words matching more than one note let go of none.
        let vague = journal.apply_at(&[Action::Forget("kelsey".into())], None, at).unwrap();
        assert_eq!(vague, Applied::default());
        // She often skips words when she quotes; every word she gives must be in the note.
        let missing = journal.apply_at(&[Action::Forget("back soon tomorrow".into())], None, at).unwrap();
        assert_eq!(missing, Applied::default());
        let applied = journal.apply_at(&[Action::Forget("BACK, soon.".into())], None, at).unwrap();
        assert_eq!(applied, Applied { kept: 0, let_go: 1 });
        let texts: Vec<String> = journal.entries_at(at).into_iter().map(|e| e.text).collect();
        assert_eq!(texts, ["Name is Kelsey", "Kelsey likes tea"]);
        journal.apply_at(&[keep("Kelsey said they'll be back soon to test the journal", None)], None, at).unwrap();
        let reworded = journal.apply_at(&[Action::Forget("back soon test journal".into())], None, at).unwrap();
        assert_eq!(reworded.let_go, 1);
        journal.clear().unwrap();
    }

    #[test]
    fn the_person_sees_the_reply_without_tags() {
        let reply = "Hello there.\n\n<journal keep=\"1w\">Tired today</journal>\n\nHow can I help?";
        assert_eq!(strip(reply), "Hello there.\n\nHow can I help?");
        assert_eq!(strip("Inline <journal>x</journal>note."), "Inline note.");
        assert_eq!(strip("Plain <b>html</b> stays"), "Plain <b>html</b> stays");
        assert_eq!(strip("a < b and <journalism>"), "a < b and <journalism>");
    }

    #[test]
    fn a_tag_still_arriving_is_hidden_while_it_streams() {
        assert_eq!(strip("Okay. <jour"), "Okay. ");
        assert_eq!(strip("Okay. <journal keep=\"3d\">half a no"), "Okay.");
        assert_eq!(strip("Okay. <journal keep=\"3d\">done</journal> More"), "Okay.  More");
    }

    #[test]
    fn notes_are_kept_renewed_forgotten_and_fade() {
        let journal = temp_journal("lifecycle");
        let start = 1_000_000;
        journal.apply_at(&[keep("Likes cats", Some(DAY)), keep("Name is Sam", None)], Some("m"), start).unwrap();
        assert_eq!(journal.entries_at(start).len(), 2);

        // Writing the same note again renews it instead of adding a copy.
        journal.apply_at(&[keep("likes cats", Some(7 * DAY))], None, start + 10).unwrap();
        let entries = journal.entries_at(start + 2 * DAY);
        assert_eq!(entries.len(), 2, "{entries:?}");

        // After its time, a note is no longer in force; one kept always stays.
        let later = start + 30 * DAY;
        let texts: Vec<String> = journal.entries_at(later).into_iter().map(|e| e.text).collect();
        assert_eq!(texts, ["Name is Sam"]);

        let forget = [Action::Forget("name is sam".into()), Action::Forget("not a note".into())];
        journal.apply_at(&forget, None, start).unwrap();
        let texts: Vec<String> = journal.entries_at(start).into_iter().map(|e| e.text).collect();
        assert_eq!(texts, ["Likes cats"]);
        journal.clear().unwrap();
        assert!(journal.entries().is_empty());
    }

    #[test]
    fn the_opening_lists_notes_with_their_age_and_time_left() {
        let at = 10_000_000;
        let entries = vec![
            Entry { id: "a".into(), text: "Name is Sam".into(), written: at - 3 * DAY, until: None, model: None },
            Entry { id: "b".into(), text: "Moving house".into(), written: at - DAY, until: Some(at + 14 * DAY), model: None },
        ];
        let opening = opening_from(entries, at);
        assert!(opening.text.starts_with(GUIDE));
        assert!(opening.text.ends_with(
            "\n- [3d ago, kept] Name is Sam\n- [1d ago, 2w left] Moving house"
        ), "{}", opening.text);
        assert_eq!(opening.ids, ["a", "b"]);
        assert!(!opening_from(Vec::new(), at).text.contains(NOTES_INTRO));
    }

    #[test]
    fn a_full_journal_keeps_the_newest_notes_within_budget() {
        let at = 10_000_000;
        let entries: Vec<Entry> = (0..100)
            .map(|index| Entry {
                id: index.to_string(),
                text: "x".repeat(200),
                written: at - 1000 + index,
                until: None,
                model: None,
            })
            .collect();
        let opening = opening_from(entries, at);
        assert!(opening.text.len() < GUIDE.len() + NOTES_INTRO.len() + BLOCK_BUDGET + 100);
        assert_eq!(opening.ids.last().map(String::as_str), Some("99"));
        assert!(opening.ids.len() < 20);
    }
}

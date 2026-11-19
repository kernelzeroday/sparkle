//! State for the `/prompts` browser. Drawing lives in `view`, and choosing a prompt in `app`.

use std::path::Path;

use sparkle_core::library::{self, PromptPack};
use sparkle_core::personas;

const ALL: &str = "All";
const BUILT_IN: &str = "Sparkle";
const UNCATEGORIZED: &str = "Uncategorized";

/// How much of a prompt the preview pane reads.
const PREVIEW_CHARS: usize = 8000;

/// Where a row's text comes from.
enum Text {
    BuiltIn(&'static str),
    /// An index into the pack's entries, read only when the row is previewed.
    Packed(usize),
}

/// One row in the browser.
pub struct Entry {
    pub name: String,
    pub title: String,
    pub group: String,
    /// The source collection, for uncategorized prompts.
    pub collection: Option<String>,
    /// Date and origin, for library prompts.
    pub note: String,
    pub bytes: usize,
    text: Text,
    /// Lowercase name and title, so that filtering thousands of rows stays instant.
    search: String,
}

impl Entry {
    fn new(name: String, title: String, group: String, note: String, bytes: usize, text: Text) -> Self {
        let search = format!("{name} {title}").to_lowercase();
        Self { name, title, group, collection: None, note, bytes, text, search }
    }
}

fn note(date: &str, origin: &str) -> String {
    if date.is_empty() { origin.to_string() } else { format!("{date} · {origin}") }
}

fn built_in_entries() -> Vec<Entry> {
    let group = || BUILT_IN.to_string();
    let off = library::OFF.to_string();
    let title = "No system prompt".to_string();
    let mut entries = vec![Entry::new(off, title, group(), String::new(), 0, Text::BuiltIn(""))];
    entries.extend(personas::PERSONAS.iter().map(|persona| {
        let (name, title) = (persona.name.to_string(), persona.summary.to_string());
        let bytes = persona.prompt.len();
        Entry::new(name, title, group(), "persona".into(), bytes, Text::BuiltIn(persona.prompt))
    }));
    entries.extend(library::PROMPTS.iter().map(|prompt| {
        Entry::new(
            prompt.id.to_string(),
            prompt.title.to_string(),
            prompt.family_name().to_string(),
            note(prompt.date, prompt.origin.label()),
            prompt.text.len(),
            Text::BuiltIn(prompt.text),
        )
    }));
    entries
}

pub struct Picker {
    pub entries: Vec<Entry>,
    pub groups: Vec<String>,
    pub group: usize,
    /// Source collections under Uncategorized. Position 0 of `collection` means all of them.
    pub collections: Vec<String>,
    pub collection: usize,
    pub filter: String,
    /// Position within the rows currently visible.
    pub cursor: usize,
    /// The path being typed, while choosing an external file.
    pub path: Option<String>,
    pub error: Option<String>,
    /// The start of the highlighted prompt's text.
    pub preview: String,
    previewed: Option<usize>,
    rows: Vec<usize>,
    pack: Option<PromptPack>,
}

impl Picker {
    /// Opens with the cursor on the prompt in use, when it is in the catalog.
    pub fn open(active: Option<&str>, pack: Option<&Path>) -> Self {
        let mut entries = built_in_entries();
        let mut groups: Vec<String> = [ALL, BUILT_IN].map(str::to_string).to_vec();
        groups.extend(library::FAMILIES.iter().map(|(_, name)| (*name).to_string()));

        let pack = pack.and_then(PromptPack::open);
        let mut collections = Vec::new();
        for (index, item) in pack.iter().flat_map(|pack| pack.entries.iter().enumerate()) {
            let mut entry = Entry::new(
                item.id.clone(),
                item.title.clone(),
                item.group.clone(),
                note(&item.date, &item.origin),
                item.bytes,
                Text::Packed(index),
            );
            entry.collection = item.collection().map(str::to_string);
            if let Some(collection) = &entry.collection
                && !collections.contains(collection)
            {
                collections.push(collection.clone());
            }
            if item.group != UNCATEGORIZED && !groups.contains(&item.group) {
                groups.push(item.group.clone());
            }
            entries.push(entry);
        }
        collections.sort();
        if !collections.is_empty() && entries.iter().any(|entry| entry.group == UNCATEGORIZED) {
            groups.push(UNCATEGORIZED.to_string());
        }

        let mut picker = Self {
            entries,
            groups,
            group: 0,
            collections,
            collection: 0,
            filter: String::new(),
            cursor: 0,
            path: None,
            error: None,
            preview: String::new(),
            previewed: None,
            rows: Vec::new(),
            pack,
        };
        picker.update_rows();
        let wanted = active.unwrap_or(library::OFF);
        let found = picker.rows.iter().position(|&row| picker.entries[row].name == wanted);
        picker.cursor = found.unwrap_or(0);
        picker.refresh_preview();
        picker
    }

    /// Indexes of the entries in the current group that match every word of the filter.
    pub fn visible(&self) -> &[usize] {
        &self.rows
    }

    fn update_rows(&mut self) {
        let group = self.groups[self.group].as_str();
        let collection = self.collection.checked_sub(1).map(|index| &self.collections[index]);
        let filter = self.filter.to_lowercase();
        let words: Vec<&str> = filter.split_whitespace().collect();
        self.rows = (0..self.entries.len())
            .filter(|&index| {
                let entry = &self.entries[index];
                (group == ALL || entry.group == group)
                    && collection.is_none_or(|wanted| entry.collection.as_ref() == Some(wanted))
                    && words.iter().all(|word| entry.search.contains(word))
            })
            .collect();
        self.cursor = 0;
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.rows.get(self.cursor).map(|&index| &self.entries[index])
    }

    /// Loads the highlighted prompt's text, reading from disk only when the row changes.
    pub fn refresh_preview(&mut self) {
        let row = self.rows.get(self.cursor).copied();
        if row == self.previewed {
            return;
        }
        self.previewed = row;
        let Some(entry) = row.map(|index| &self.entries[index]) else {
            self.preview.clear();
            return;
        };
        self.preview = match entry.text {
            Text::BuiltIn(text) => text.chars().take(PREVIEW_CHARS).collect(),
            Text::Packed(index) => self
                .pack
                .as_ref()
                .and_then(|pack| pack.read(pack.entries.get(index)?).ok())
                .map_or_else(String::new, |text| text.chars().take(PREVIEW_CHARS).collect()),
        };
    }

    /// Moves the cursor, stopping at either end of the list.
    pub fn move_by(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    /// Switches to the next or previous vendor, wrapping around.
    pub fn switch_group(&mut self, delta: isize) {
        let count = self.groups.len() as isize;
        self.group = (self.group as isize + delta).rem_euclid(count) as usize;
        self.collection = 0;
        self.update_rows();
    }

    /// True while the source collections of the Uncategorized tab are on screen.
    pub fn has_collections(&self) -> bool {
        self.groups[self.group] == UNCATEGORIZED
    }

    /// Switches to the next or previous source collection, wrapping around.
    pub fn switch_collection(&mut self, delta: isize) {
        let count = self.collections.len() as isize + 1;
        self.collection = (self.collection as isize + delta).rem_euclid(count) as usize;
        self.update_rows();
    }

    pub fn push_filter(&mut self, text: &str) {
        self.filter.push_str(text);
        self.update_rows();
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.update_rows();
    }
}

/// Extends a typed path as far as the files on disk agree.
pub fn complete_path(typed: &str) -> String {
    let expanded = library::expand_home(typed).to_string_lossy().into_owned();
    let (directory, stem) = match expanded.rfind('/') {
        Some(slash) => expanded.split_at(slash + 1),
        None => ("", expanded.as_str()),
    };
    let listing = if directory.is_empty() { Path::new(".") } else { Path::new(directory) };
    let Ok(children) = std::fs::read_dir(listing) else { return typed.to_string() };
    let mut names: Vec<(String, bool)> = children
        .flatten()
        .filter_map(|child| {
            let name = child.file_name().into_string().ok()?;
            let hidden = name.starts_with('.') && !stem.starts_with('.');
            (name.starts_with(stem) && !hidden).then(|| (name, child.path().is_dir()))
        })
        .collect();
    names.sort();
    let Some((first, is_directory)) = names.first() else { return typed.to_string() };
    if names.len() == 1 {
        let suffix = if *is_directory { "/" } else { "" };
        return format!("{directory}{first}{suffix}");
    }
    let shared = names.iter().fold(first.len(), |length, (name, _)| {
        first.bytes().zip(name.bytes()).take(length).take_while(|(a, b)| a == b).count()
    });
    // A shared prefix can end inside a multi-byte character.
    let shared = (0..=shared).rev().find(|&end| first.is_char_boundary(end)).unwrap_or(0);
    format!("{directory}{}", &first[..shared])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn open(active: Option<&str>) -> Picker {
        Picker::open(active, None)
    }

    fn pack(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("sparkle-pack-{}-{name}", std::process::id()));
        std::fs::create_dir_all(directory.join("prompts/minimax")).unwrap();
        std::fs::create_dir_all(directory.join("prompts/uncategorized/fabric")).unwrap();
        std::fs::create_dir_all(directory.join("prompts/uncategorized/langgpt")).unwrap();
        std::fs::write(
            directory.join("index.tsv"),
            "# sparkle prompt pack 1\n\
             minimax/m3\tMiniMax M3\tMiniMax\t2026\tcaptured\t7\ta.md\n\
             uncategorized/fabric/summarize\tSummarize\tUncategorized\t\tcommunity\t9\tb.md\n\
             uncategorized/langgpt/poet\tPoet\tUncategorized\t\tcommunity\t4\tc.md\n",
        )
        .unwrap();
        std::fs::write(directory.join("prompts/minimax/m3.md"), "M3 text").unwrap();
        std::fs::write(directory.join("prompts/uncategorized/fabric/summarize.md"), "Summarize").unwrap();
        std::fs::write(directory.join("prompts/uncategorized/langgpt/poet.md"), "Poet").unwrap();
        directory
    }

    #[test]
    fn the_catalog_lists_off_personas_and_every_library_prompt() {
        let picker = open(None);
        assert_eq!(picker.entries[0].name, "off");
        assert_eq!(picker.entries.len(), 1 + personas::PERSONAS.len() + library::PROMPTS.len());
        assert_eq!(picker.selected().unwrap().name, "off");
        assert!(!picker.groups.iter().any(|group| group == UNCATEGORIZED));
    }

    #[test]
    fn pack_prompts_join_their_vendor_or_uncategorized() {
        let mut picker = Picker::open(None, Some(&pack("groups")));
        assert!(picker.groups.iter().any(|group| group == "MiniMax"));
        assert_eq!(picker.groups.last().unwrap(), UNCATEGORIZED);
        assert_eq!(picker.collections, ["fabric", "langgpt"]);

        picker.switch_group(-1);
        assert!(picker.has_collections());
        assert_eq!(picker.visible().len(), 2);
        picker.switch_collection(1);
        assert_eq!(picker.selected().unwrap().name, "uncategorized/fabric/summarize");
        picker.switch_collection(1);
        assert_eq!(picker.selected().unwrap().name, "uncategorized/langgpt/poet");
        picker.switch_collection(1);
        assert_eq!(picker.visible().len(), 2, "wraps back to all sources");
    }

    #[test]
    fn previews_read_pack_prompts_from_disk() {
        let mut picker = Picker::open(Some("minimax/m3"), Some(&pack("preview")));
        assert_eq!(picker.selected().unwrap().name, "minimax/m3");
        assert_eq!(picker.preview, "M3 text");
        picker.push_filter("poet");
        picker.refresh_preview();
        assert_eq!(picker.preview, "Poet");
        picker.push_filter("nothing");
        picker.refresh_preview();
        assert_eq!(picker.preview, "");
    }

    #[test]
    fn the_cursor_opens_on_the_active_prompt() {
        let picker = open(Some("openai/gpt-3.5"));
        assert_eq!(picker.selected().unwrap().name, "openai/gpt-3.5");
        assert_eq!(open(Some("my-file.md")).selected().unwrap().name, "off");
    }

    #[test]
    fn filtering_needs_every_word_and_resets_the_cursor() {
        let mut picker = open(Some("openai/gpt-3.5"));
        picker.push_filter("GROK 4.7");
        assert_eq!(picker.cursor, 0);
        assert_eq!(picker.selected().unwrap().name, "xai/grok-4.7");
        picker.push_filter(" nothing-matches");
        assert!(picker.selected().is_none());
        picker.move_by(1);
        assert_eq!(picker.cursor, 0);
    }

    #[test]
    fn groups_limit_the_list_and_wrap_around() {
        let mut picker = open(None);
        picker.switch_group(1);
        assert_eq!(picker.groups[picker.group], "Sparkle");
        assert_eq!(picker.visible().len(), 1 + personas::PERSONAS.len());
        picker.switch_group(1);
        assert_eq!(picker.groups[picker.group], "OpenAI");
        assert!(picker.visible().iter().all(|&i| picker.entries[i].name.starts_with("openai/")));
        picker.switch_group(-2);
        picker.switch_group(-1);
        assert_eq!(picker.group, picker.groups.len() - 1);
    }

    #[test]
    fn every_vendor_group_has_prompts() {
        let mut picker = open(None);
        for _ in 0..picker.groups.len() {
            assert!(!picker.visible().is_empty(), "{} is empty", picker.groups[picker.group]);
            picker.switch_group(1);
        }
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut picker = open(None);
        picker.move_by(-5);
        assert_eq!(picker.cursor, 0);
        picker.move_by(100_000);
        assert_eq!(picker.cursor, picker.visible().len() - 1);
        picker.move_by(isize::MIN);
        assert_eq!(picker.cursor, 0);
    }

    #[test]
    fn paths_complete_from_the_disk() {
        let directory = std::env::temp_dir().join(format!("sparkle-picker-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("nested")).unwrap();
        std::fs::write(directory.join("rules-one.md"), "a").unwrap();
        std::fs::write(directory.join("rules-two.md"), "b").unwrap();
        let base = format!("{}/", directory.display());

        assert_eq!(complete_path(&format!("{base}ru")), format!("{base}rules-"));
        assert_eq!(complete_path(&format!("{base}rules-o")), format!("{base}rules-one.md"));
        assert_eq!(complete_path(&format!("{base}nes")), format!("{base}nested/"));
        assert_eq!(complete_path(&format!("{base}zzz")), format!("{base}zzz"));
    }
}

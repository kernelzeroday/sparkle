//! State for the `/prompts` browser. Drawing lives in `view`, and choosing a prompt in `app`.

use std::path::Path;

use sparkle_core::{library, personas};

const ALL: &str = "All";
const BUILT_IN: &str = "Sparkle";

/// One row in the browser.
pub struct Entry {
    pub name: String,
    pub title: String,
    pub group: &'static str,
    /// Date and origin, for library prompts.
    pub note: String,
    pub text: &'static str,
}

fn entries() -> Vec<Entry> {
    let mut entries = vec![Entry {
        name: library::OFF.to_string(),
        title: "No system prompt".to_string(),
        group: BUILT_IN,
        note: String::new(),
        text: "",
    }];
    entries.extend(personas::PERSONAS.iter().map(|persona| Entry {
        name: persona.name.to_string(),
        title: persona.summary.to_string(),
        group: BUILT_IN,
        note: "persona".to_string(),
        text: persona.prompt,
    }));
    entries.extend(library::PROMPTS.iter().map(|prompt| Entry {
        name: prompt.id.to_string(),
        title: prompt.title.to_string(),
        group: prompt.family_name(),
        note: format!("{} · {}", prompt.date, prompt.origin.label()),
        text: prompt.text,
    }));
    entries
}

pub struct Picker {
    pub entries: Vec<Entry>,
    pub groups: Vec<&'static str>,
    pub group: usize,
    pub filter: String,
    /// Position within the rows currently visible.
    pub cursor: usize,
    /// The path being typed, while choosing an external file.
    pub path: Option<String>,
    pub error: Option<String>,
}

impl Picker {
    /// Opens with the cursor on the prompt in use, when it is in the catalog.
    pub fn open(active: Option<&str>) -> Self {
        let entries = entries();
        let mut groups = vec![ALL, BUILT_IN];
        groups.extend(library::FAMILIES.iter().map(|(_, name)| *name));
        let wanted = active.unwrap_or(library::OFF);
        let cursor = entries.iter().position(|entry| entry.name == wanted).unwrap_or(0);
        Self { entries, groups, group: 0, filter: String::new(), cursor, path: None, error: None }
    }

    /// Indexes of the entries in the current group that match every word of the filter.
    pub fn visible(&self) -> Vec<usize> {
        let group = self.groups[self.group];
        let filter = self.filter.to_lowercase();
        let words: Vec<&str> = filter.split_whitespace().collect();
        (0..self.entries.len())
            .filter(|&index| {
                let entry = &self.entries[index];
                let haystack = format!("{} {}", entry.name, entry.title).to_lowercase();
                (group == ALL || entry.group == group)
                    && words.iter().all(|word| haystack.contains(word))
            })
            .collect()
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.visible().get(self.cursor).map(|&index| &self.entries[index])
    }

    /// Moves the cursor, stopping at either end of the list.
    pub fn move_by(&mut self, delta: isize) {
        let last = self.visible().len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    /// Switches to the next or previous vendor, wrapping around.
    pub fn switch_group(&mut self, delta: isize) {
        let count = self.groups.len() as isize;
        self.group = (self.group as isize + delta).rem_euclid(count) as usize;
        self.cursor = 0;
    }

    pub fn push_filter(&mut self, text: &str) {
        self.filter.push_str(text);
        self.cursor = 0;
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.cursor = 0;
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

    #[test]
    fn the_catalog_lists_off_personas_and_every_library_prompt() {
        let picker = Picker::open(None);
        assert_eq!(picker.entries[0].name, "off");
        assert_eq!(picker.entries.len(), 1 + personas::PERSONAS.len() + library::PROMPTS.len());
        assert_eq!(picker.selected().unwrap().name, "off");
    }

    #[test]
    fn the_cursor_opens_on_the_active_prompt() {
        let picker = Picker::open(Some("openai/gpt-3.5"));
        assert_eq!(picker.selected().unwrap().name, "openai/gpt-3.5");
        assert_eq!(Picker::open(Some("my-file.md")).selected().unwrap().name, "off");
    }

    #[test]
    fn filtering_needs_every_word_and_resets_the_cursor() {
        let mut picker = Picker::open(Some("openai/gpt-3.5"));
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
        let mut picker = Picker::open(None);
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
        let mut picker = Picker::open(None);
        for _ in 0..picker.groups.len() {
            assert!(!picker.visible().is_empty(), "{} is empty", picker.groups[picker.group]);
            picker.switch_group(1);
        }
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut picker = Picker::open(None);
        picker.move_by(-5);
        assert_eq!(picker.cursor, 0);
        picker.move_by(100_000);
        assert_eq!(picker.cursor, picker.visible().len() - 1);
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

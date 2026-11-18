//! System prompts built into sparkle, external prompt files, and the saved default.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Error, Result, personas};

#[path = "library_catalog.rs"]
mod catalog;

pub use catalog::{FAMILIES, PROMPTS};

/// The value that means "no system prompt" wherever a prompt can be named.
pub const OFF: &str = "off";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Released by the vendor.
    Published,
    /// Extracted by the community, so it may be incomplete.
    Captured,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Published => "published",
            Origin::Captured => "captured",
        }
    }
}

#[derive(Debug)]
pub struct Prompt {
    pub id: &'static str,
    pub title: &'static str,
    pub date: &'static str,
    pub origin: Origin,
    pub source: &'static str,
    pub text: &'static str,
}

impl Prompt {
    /// The part of the id before the slash, such as `openai`.
    pub fn family(&self) -> &'static str {
        self.id.split_once('/').map_or(self.id, |(family, _)| family)
    }

    /// The part of the id after the slash, such as `gpt-4o-2024-05`.
    pub fn name(&self) -> &'static str {
        self.id.split_once('/').map_or(self.id, |(_, name)| name)
    }

    /// The vendor's display name, such as `OpenAI`.
    pub fn family_name(&self) -> &'static str {
        let family = self.family();
        FAMILIES.iter().find(|(key, _)| *key == family).map_or(family, |(_, name)| *name)
    }
}

/// Finds a prompt by its full id, or by its name when only one family uses that name.
pub fn find(name: &str) -> Option<&'static Prompt> {
    if let Some(prompt) = PROMPTS.iter().find(|prompt| prompt.id.eq_ignore_ascii_case(name)) {
        return Some(prompt);
    }
    let mut matches = PROMPTS.iter().filter(|prompt| prompt.name().eq_ignore_ascii_case(name));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// A system prompt ready to send, with the short name to show for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub label: String,
    pub text: String,
}

/// Expands a leading `~/` to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Reads an external prompt file.
pub fn load_file(path: &Path) -> Result<Loaded> {
    let shown = path.display();
    let bytes = std::fs::read(path)
        .map_err(|error| Error::Config(format!("could not read {shown}: {error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Config(format!("{shown} is not UTF-8 text")))?;
    if text.trim().is_empty() {
        return Err(Error::Config(format!("{shown} is empty")));
    }
    let label = path.file_name().map_or_else(|| shown.to_string(), |n| n.to_string_lossy().into());
    Ok(Loaded { label, text })
}

/// Loads a persona, a library prompt or a file, in that order.
pub fn load(name: &str) -> Result<Loaded> {
    if let Some(persona) = personas::find(name) {
        return Ok(Loaded { label: persona.name.to_string(), text: persona.prompt.to_string() });
    }
    if let Some(prompt) = find(name) {
        return Ok(Loaded { label: prompt.id.to_string(), text: fill_date(prompt.text, &today()) });
    }
    let path = expand_home(name);
    if path.is_file() {
        return load_file(&path);
    }
    Err(Error::Config(format!(
        "unknown system prompt '{name}'. Run `sparkle prompts` to see the list, or give a file path"
    )))
}

/// The form of a prompt name to save as the default. Files are stored as absolute paths so the
/// default keeps working from any directory.
pub fn storable_name(name: &str) -> Result<String> {
    let loaded = load(name)?;
    if personas::find(name).is_some() || find(name).is_some() {
        return Ok(loaded.label);
    }
    let path = expand_home(name);
    let absolute = path.canonicalize().map_err(|error| {
        Error::Config(format!("could not resolve {}: {error}", path.display()))
    })?;
    Ok(absolute.to_string_lossy().into_owned())
}

/// The saved default prompt name, if there is one.
pub fn saved_default(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    Some(text.trim().to_string()).filter(|name| !name.is_empty())
}

/// Saves the default prompt name, or removes it when given `None`.
pub fn save_default(file: &Path, name: Option<&str>) -> Result<()> {
    match name {
        Some(name) => {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(file, format!("{name}\n"))?;
        }
        None => match std::fs::remove_file(file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        },
    }
    Ok(())
}

/// Replaces the date placeholders vendors leave in their prompts.
fn fill_date(text: &str, date: &str) -> String {
    ["{{currentDateTime}}", "{{current_date}}", "{CURRENT_DATE}", "{CURRENTDATETIME}"]
        .iter()
        .fold(text.to_string(), |text, placeholder| text.replace(placeholder, date))
}

/// Today's UTC date as `YYYY-MM-DD`.
fn today() -> String {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_secs());
    civil_date(seconds / 86_400)
}

/// Converts days since 1970-01-01 to a calendar date.
fn civil_date(days: u64) -> String {
    // Counts from 0000-03-01, so that a leap day falls at the end of each 400-year era.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn temp_path(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("sparkle-library-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join(name)
    }

    #[test]
    fn every_prompt_has_a_unique_id_a_known_family_and_text() {
        let mut seen = HashSet::new();
        for prompt in PROMPTS {
            assert!(seen.insert(prompt.id), "{} is listed twice", prompt.id);
            assert!(FAMILIES.iter().any(|(key, _)| *key == prompt.family()), "{}", prompt.id);
            assert!(!prompt.text.trim().is_empty(), "{} is empty", prompt.id);
        }
    }

    #[test]
    fn the_major_model_series_are_covered_including_legacy_ones() {
        for id in [
            "openai/gpt-3.5",
            "openai/gpt-4o-2024-05",
            "openai/gpt-5.6-sol",
            "anthropic/claude-3-opus",
            "anthropic/claude-opus-5.5",
            "google/gemini-1.5-2024-04",
            "xai/grok-1",
            "meta/llama-3",
            "mistral/le-chat-2024-11",
            "deepseek/deepseek-r1",
        ] {
            assert!(find(id).is_some(), "{id} is missing");
        }
    }

    #[test]
    fn vendored_prompts_do_not_identify_the_people_who_captured_them() {
        for prompt in PROMPTS {
            let text = prompt.text.to_lowercase();
            for private in ["sgeir", "thor johnson", "hafnarfj", "reykjav", "lebanon, wa"] {
                assert!(!text.contains(private), "{} contains {private}", prompt.id);
            }
        }
    }

    #[test]
    fn prompts_are_found_by_id_or_by_an_unambiguous_name() {
        assert_eq!(find("openai/gpt-3.5").unwrap().id, "openai/gpt-3.5");
        assert_eq!(find("OPENAI/GPT-3.5").unwrap().id, "openai/gpt-3.5");
        assert_eq!(find("gpt-3.5").unwrap().id, "openai/gpt-3.5");
        assert_eq!(find("gpt-3.5").unwrap().family_name(), "OpenAI");
        assert!(find("no-such-prompt").is_none());
    }

    #[test]
    fn loading_prefers_personas_then_the_library_then_files() {
        assert_eq!(load("concise").unwrap().label, "concise");
        assert_eq!(load("gpt-3.5").unwrap().label, "openai/gpt-3.5");

        let path = temp_path("rules.md");
        std::fs::write(&path, "Be kind.").unwrap();
        let loaded = load(path.to_str().unwrap()).unwrap();
        assert_eq!((loaded.label.as_str(), loaded.text.as_str()), ("rules.md", "Be kind."));
        assert!(load("/nonexistent/prompt.md").unwrap_err().to_string().contains("unknown"));
    }

    #[test]
    fn unusable_files_are_explained() {
        let empty = temp_path("empty.md");
        std::fs::write(&empty, "  \n").unwrap();
        assert!(load_file(&empty).unwrap_err().to_string().contains("is empty"));

        let binary = temp_path("binary.md");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        assert!(load_file(&binary).unwrap_err().to_string().contains("not UTF-8"));
    }

    #[test]
    fn the_default_survives_a_round_trip_and_can_be_cleared() {
        let file = temp_path("nested/default_prompt.txt");
        assert_eq!(saved_default(&file), None);
        save_default(&file, Some("openai/gpt-3.5")).unwrap();
        assert_eq!(saved_default(&file).as_deref(), Some("openai/gpt-3.5"));
        save_default(&file, None).unwrap();
        assert_eq!(saved_default(&file), None);
        save_default(&file, None).unwrap();
    }

    #[test]
    fn stored_names_are_canonical() {
        assert_eq!(storable_name("gpt-3.5").unwrap(), "openai/gpt-3.5");
        let path = temp_path("stored.md");
        std::fs::write(&path, "text").unwrap();
        let stored = storable_name(path.to_str().unwrap()).unwrap();
        assert!(Path::new(&stored).is_absolute(), "{stored}");
        assert!(storable_name("no-such-prompt").is_err());
    }

    #[test]
    fn date_placeholders_are_filled() {
        assert_eq!(fill_date("Today is {{currentDateTime}}.", "2026-11-18"), "Today is 2026-11-18.");
        assert_eq!(fill_date("{CURRENT_DATE} {{current_date}}", "D"), "D D");
        assert!(!load("anthropic/claude-3-opus").unwrap().text.contains("{{currentDateTime}}"));
    }

    #[test]
    fn calendar_dates_are_correct_across_leap_years() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(19_782), "2024-02-29");
        assert_eq!(civil_date(19_783), "2024-03-01");
        assert_eq!(civil_date(20_775), "2026-11-18");
    }
}

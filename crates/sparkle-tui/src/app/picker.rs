//! Key handling for the `/prompts` browser and its file-path input.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sparkle_core::library;

use crate::picker;

use super::App;

impl App {
    pub(super) fn on_picker_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            self.picker = None;
        } else if self.picker.as_ref().is_some_and(|picker| picker.path.is_some()) {
            self.on_path_key(key, control);
        } else {
            self.on_browse_key(key, control);
        }
    }

    fn on_browse_key(&mut self, key: KeyEvent, control: bool) {
        let Some(picker) = &mut self.picker else { return };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Enter => {
                let Some(name) = picker.selected().map(|entry| entry.name.clone()) else { return };
                match self.use_prompt(&name) {
                    Ok(()) => self.picker = None,
                    Err(error) => self.show_picker_error(error),
                }
            }
            KeyCode::Char('d') if control => {
                let Some(name) = picker.selected().map(|entry| entry.name.clone()) else { return };
                // Pressing it again on the current default removes it.
                let is_default = self.default_prompt.as_deref() == Some(name.as_str());
                let target = if is_default { library::OFF } else { name.as_str() };
                if let Err(error) = self.set_default_prompt(target) {
                    self.show_picker_error(error);
                }
            }
            KeyCode::Char('o') if control => picker.path = Some(String::new()),
            KeyCode::Up => picker.move_by(-1),
            KeyCode::Down => picker.move_by(1),
            KeyCode::PageUp => picker.move_by(-10),
            KeyCode::PageDown => picker.move_by(10),
            KeyCode::Home => picker.move_by(isize::MIN),
            KeyCode::End => picker.move_by(isize::MAX),
            KeyCode::Left => picker.switch_group(-1),
            KeyCode::Right => picker.switch_group(1),
            KeyCode::BackTab if picker.has_collections() => picker.switch_collection(-1),
            KeyCode::Tab if picker.has_collections() => picker.switch_collection(1),
            KeyCode::BackTab => picker.switch_group(-1),
            KeyCode::Tab => picker.switch_group(1),
            KeyCode::Backspace => picker.pop_filter(),
            KeyCode::Char(typed) if !control => picker.push_filter(typed.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        if let Some(picker) = &mut self.picker {
            picker.refresh_preview();
        }
    }

    fn on_path_key(&mut self, key: KeyEvent, control: bool) {
        let Some(picker) = &mut self.picker else { return };
        let Some(path) = &mut picker.path else { return };
        picker.error = None;
        match key.code {
            KeyCode::Esc => picker.path = None,
            KeyCode::Tab => *path = picker::complete_path(path),
            KeyCode::Backspace => {
                path.pop();
            }
            KeyCode::Enter => {
                let chosen = path.trim().to_string();
                self.finish_file_choice(&chosen, false);
            }
            KeyCode::Char('d') if control => {
                let chosen = path.trim().to_string();
                self.finish_file_choice(&chosen, true);
            }
            KeyCode::Char(typed) if !control => path.push(typed),
            _ => {}
        }
    }

    /// Closes the browser once a file is in use, or keeps it open to show what went wrong.
    fn finish_file_choice(&mut self, path: &str, make_default: bool) {
        if path.is_empty() {
            return;
        }
        match self.choose_file(path, make_default) {
            Ok(()) => self.picker = None,
            Err(error) => self.show_picker_error(error),
        }
    }

    /// Uses an external file as the system prompt, and optionally saves it as the default.
    pub(super) fn choose_file(&mut self, path: &str, make_default: bool) -> Result<(), String> {
        let file = library::expand_home(path);
        let loaded = library::load_file(&file).map_err(|error| error.to_string())?;
        self.apply_prompt(loaded);
        if make_default {
            // Saving replaces the notice, so a size warning has to be carried across.
            let warning = self.context_warning();
            let absolute = file.canonicalize().map_err(|error| error.to_string())?;
            self.set_default_prompt(&absolute.to_string_lossy())?;
            if let Some((_, warning)) = warning {
                self.notice = Some(warning);
            }
        }
        Ok(())
    }

    fn show_picker_error(&mut self, error: String) {
        if let Some(picker) = &mut self.picker {
            picker.error = Some(error);
        }
    }

}

//! Slash-command handling and prompt/persona selection.

use sparkle_core::context::size_label;
use sparkle_core::{Usage, library, personas};

use crate::picker::Picker;

use super::{Action, App, COMMANDS, EFFORTS, Phase, Turn, new_input};

impl App {
    pub(super) fn cycle_effort(&mut self) {
        let current = self.options.effort.as_deref();
        let index = EFFORTS.iter().position(|effort| *effort == current).unwrap_or(0);
        let next = EFFORTS[(index + 1) % EFFORTS.len()];
        self.options.effort = next.map(str::to_string);
        self.notice = Some(format!("Reasoning effort: {}", next.unwrap_or("auto")));
    }

    pub(super) fn submit(&mut self) {
        if self.busy() {
            let activity = if self.phase == Phase::Compacting { "compacting" } else { "replying" };
            self.notice = Some(format!("Still {activity}. Press esc to stop."));
            return;
        }
        let text = self.input.lines().join("\n");
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let text = text.to_string();
        self.input = new_input(&self.theme);
        self.scroll = 0;
        // A first word like /etc/hosts is a path in a message, not a command.
        let first_word = text.split_whitespace().next().unwrap_or("");
        if first_word.starts_with('/') && !first_word[1..].contains('/') {
            self.command(&text);
        } else {
            self.send(text);
        }
    }

    fn command(&mut self, text: &str) {
        let (name, argument) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let argument = argument.trim();
        match name {
            "/help" => {
                let mut help: Vec<String> =
                    COMMANDS.iter().map(|(name, about)| format!("{name:<10} {about}")).collect();
                help.push(String::new());
                help.push("enter send   shift+enter or ctrl+j newline   esc stop".into());
                help.push("ctrl+t thinking   ctrl+o compaction summary   shift+tab effort".into());
                help.push("pgup/pgdn scroll   tab complete command   ctrl+c/d twice exit".into());
                self.turns.push(Turn::notice(help.join("\n")));
            }
            "/new" | "/clear" => {
                self.turns.clear();
                self.conversation_id = None;
                self.reset_title();
                self.total_usage = Usage::default();
                self.measured_context = None;
                self.notice = Some("Started a new conversation".into());
            }
            "/quit" | "/exit" => self.should_quit = true,
            "/model" if argument.is_empty() => {
                self.notice = Some(format!("Current model: {}", self.model));
            }
            "/model" => match (self.resolver)(argument) {
                Ok(model) => {
                    self.model = model.id;
                    self.wire_model = model.wire_name;
                    self.provider = model.provider;
                    self.prompt_pack = model.prompt_pack;
                    self.measured_context = None;
                    self.request_context_window();
                    self.notice = Some(if self.clear_unavailable_prompt() {
                        format!("Switched to {}. Its red-team prompt was cleared", self.model)
                    } else {
                        format!("Switched to {}", self.model)
                    });
                }
                Err(error) => self.notice = Some(error),
            },
            "/effort" => match argument {
                "auto" | "off" => {
                    self.options.effort = None;
                    self.notice = Some("Reasoning effort: auto".into());
                }
                "" => {
                    let effort = self.options.effort.as_deref().unwrap_or("auto");
                    self.notice = Some(format!("Reasoning effort: {effort}"));
                }
                value => {
                    self.options.effort = Some(value.to_string());
                    self.notice = Some(format!("Reasoning effort: {value}"));
                }
            },
            "/persona" => self.set_persona(argument),
            "/prompts" => self.prompts_command(argument),
            "/system" => {
                self.system = (!argument.is_empty()).then(|| argument.to_string());
                self.persona = None;
                self.measured_context = None;
                self.notice = Some(
                    if self.system.is_some() { "System prompt set" } else { "System prompt cleared" }
                        .into(),
                );
            }
            "/prefill" => {
                self.prefill = (!argument.is_empty()).then(|| argument.to_string());
                self.notice = Some(
                    if self.prefill.is_some() {
                        "The next reply will continue from your text"
                    } else {
                        "Prefill cleared"
                    }
                    .into(),
                );
            }
            "/retry" => self.retry(),
            "/edit" => {
                if self.last_reply().is_some() {
                    self.action = Some(Action::EditLastReply);
                } else {
                    self.notice = Some("There is no reply to edit yet".into());
                }
            }
            "/compact" => self.begin_compaction(argument),
            other => {
                // Keep what was typed so a mistyped command is not lost.
                let notice = format!("Unknown command {other}. Try /help");
                self.set_input(text);
                self.notice = Some(notice);
            }
        }
    }

    fn set_persona(&mut self, name: &str) {
        if name.is_empty() {
            let names: Vec<&str> = personas::PERSONAS.iter().map(|persona| persona.name).collect();
            self.notice = Some(format!("Personas: {}", names.join(", ")));
        } else if name == "off" {
            self.persona = None;
            self.system = None;
            self.measured_context = None;
            self.notice = Some("Persona cleared".into());
        } else if let Some(persona) = personas::find(name) {
            self.persona = Some(persona.name.to_string());
            self.system = Some(persona.prompt.to_string());
            self.measured_context = None;
            self.notice = Some(format!("Persona: {}", persona.name));
        } else {
            self.notice = Some(format!("Unknown persona '{name}'"));
        }
    }

    /// Red-team prompts only run on local models, so they do not follow a switch to a remote one.
    pub(super) fn clear_unavailable_prompt(&mut self) -> bool {
        let Some(name) = self.persona.as_deref().filter(|name| name.starts_with("redteam/")) else {
            return false;
        };
        if library::load(name, self.prompt_pack.as_deref()).is_ok() {
            return false;
        }
        self.persona = None;
        self.system = None;
        true
    }

    pub(super) fn prompts_command(&mut self, argument: &str) {
        let (verb, rest) = argument.split_once(char::is_whitespace).unwrap_or((argument, ""));
        match (verb, rest.trim()) {
            ("", _) => {
                let pack = self.prompt_pack.as_deref();
                self.picker = Some(Picker::open(self.persona.as_deref(), pack));
            }
            ("file", "") => {
                let pack = self.prompt_pack.as_deref();
                let mut picker = Picker::open(self.persona.as_deref(), pack);
                picker.path = Some(String::new());
                self.picker = Some(picker);
            }
            ("file", path) => {
                if let Err(error) = self.choose_file(path, false) {
                    self.notice = Some(error);
                }
            }
            ("default", "") => {
                let current = self.default_prompt.as_deref().unwrap_or("none");
                self.notice = Some(format!("Default system prompt: {current}"));
            }
            ("default", name) => {
                if let Err(error) = self.set_default_prompt(name) {
                    self.notice = Some(error);
                }
            }
            _ => self.report_prompt(argument),
        }
    }

    /// Switches prompt from a command, where any problem belongs on the notice line.
    fn report_prompt(&mut self, name: &str) {
        if let Err(error) = self.use_prompt(name) {
            self.notice = Some(error);
        }
    }

    /// Makes a named prompt or file the system prompt, and says how it fits the model.
    pub(super) fn use_prompt(&mut self, name: &str) -> Result<(), String> {
        if name == library::OFF {
            self.persona = None;
            self.system = None;
            self.measured_context = None;
            self.notice = Some("System prompt cleared".into());
            return Ok(());
        }
        let pack = self.prompt_pack.as_deref();
        let loaded = library::load(name, pack).map_err(|error| error.to_string())?;
        self.apply_prompt(loaded);
        Ok(())
    }

    pub(super) fn apply_prompt(&mut self, loaded: library::Loaded) {
        let size = size_label(loaded.text.len());
        self.system = Some(loaded.text);
        self.persona = Some(loaded.label.clone());
        self.measured_context = None;
        self.notice = Some(match self.context_warning() {
            Some((_, warning)) => format!("{}: {warning}", loaded.label),
            None => format!("System prompt: {}, {size}", loaded.label),
        });
    }

    /// Saves the prompt new chats start with. `off` removes the saved default.
    pub(super) fn set_default_prompt(&mut self, name: &str) -> Result<(), String> {
        let Some(file) = self.default_prompt_file.clone() else {
            return Err("The default system prompt cannot be saved here".into());
        };
        let stored = if name == library::OFF {
            None
        } else {
            let pack = self.prompt_pack.as_deref();
            Some(library::storable_name(name, pack).map_err(|error| error.to_string())?)
        };
        library::save_default(&file, stored.as_deref()).map_err(|error| error.to_string())?;
        self.notice = Some(match &stored {
            Some(name) => format!("New chats will start with {name}"),
            None => "New chats will start without a default system prompt".into(),
        });
        self.default_prompt = stored;
        Ok(())
    }

}

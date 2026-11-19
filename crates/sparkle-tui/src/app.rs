//! Chat state and behaviour, independent of the real terminal.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use sparkle_core::compact;
use sparkle_core::context::{self, Fit, compact_count, estimate_tokens, size_label};
use sparkle_core::library;
use sparkle_core::personas;
use sparkle_core::prefill::PrefillFilter;
use sparkle_core::title;
use sparkle_core::{
    ChatRequest, Message, Options, PromptProgress, Provider, Role, StreamEvent, Usage,
};
use sparkle_store::{NewMessage, Store, StoredMessage};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders};
use ratatui_textarea::{TextArea, WrapMode};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::picker::{self, Picker};
use crate::theme::{Icons, Theme};

pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show commands and keys"),
    ("/new", "Start a new conversation"),
    ("/model", "Switch model: /model NAME"),
    ("/effort", "Set reasoning: /effort low|medium|high|auto"),
    ("/persona", "Use a built-in persona: /persona NAME|off"),
    ("/prompts", "Browse system prompts: /prompts [NAME|off|default|file]"),
    ("/system", "Set the system prompt: /system TEXT"),
    ("/prefill", "Start the next reply with: /prefill TEXT"),
    ("/retry", "Regenerate the last reply"),
    ("/edit", "Edit the last reply in $EDITOR"),
    ("/compact", "Summarize earlier messages to free context"),
    ("/quit", "Leave the chat"),
];

const EFFORTS: [Option<&str>; 4] = [None, Some("low"), Some("medium"), Some("high")];

/// How long the second ctrl+c or ctrl+d has to arrive.
const EXIT_WINDOW: Duration = Duration::from_secs(2);

/// How long a local model may take to name a conversation.
const TITLE_TIMEOUT: Duration = Duration::from_secs(90);

/// The result of switching models.
pub struct ResolvedModel {
    pub id: String,
    pub wire_name: String,
    pub provider: Arc<dyn Provider>,
    /// The local prompt pack this model may use.
    pub prompt_pack: Option<PathBuf>,
}

pub type Resolver = Box<dyn Fn(&str) -> Result<ResolvedModel, String> + Send>;

/// Everything needed to open a chat.
pub struct ChatSession {
    pub model: ResolvedModel,
    pub resolver: Resolver,
    pub system: Option<String>,
    pub persona: Option<String>,
    pub options: Options,
    pub prefill: Option<String>,
    /// File contents sent ahead of the first message.
    pub fragments: Vec<String>,
    pub store: Option<Store>,
    pub conversation_id: Option<String>,
    pub history: Vec<StoredMessage>,
    pub theme: Theme,
    pub icons: Icons,
    pub animations: bool,
    pub show_reasoning: bool,
    /// The saved title when resuming a conversation.
    pub title: Option<String>,
    pub smart_titles: bool,
    /// Warn once a request fills this percentage of the context window.
    pub warn_percent: u8,
    /// Where the default system prompt is saved. Without one, defaults cannot be changed.
    pub default_prompt_file: Option<PathBuf>,
}

#[derive(Default)]
pub struct Turn {
    pub role: Option<Role>,
    pub text: String,
    /// Attached file contents, sent to the model but not drawn in the transcript.
    pub context: String,
    pub attached_files: usize,
    pub reasoning: String,
    pub model: String,
    pub error: Option<String>,
    pub interrupted: bool,
    pub usage: Option<Usage>,
    pub thought_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub stored_id: Option<String>,
    pub parent_id: Option<String>,
    /// True for the summary that stands in for compacted turns.
    pub summary: bool,
    pub cache: Option<(u16, bool, Vec<Line<'static>>)>,
}

impl Turn {
    fn new(role: Role, text: impl Into<String>) -> Self {
        Self { role: Some(role), text: text.into(), ..Self::default() }
    }

    fn notice(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    /// A summary replacing earlier turns. The header reaches the model but is not drawn.
    fn summary(text: impl Into<String>) -> Self {
        Self {
            context: compact::SUMMARY_HEADER.to_string(),
            summary: true,
            ..Self::new(Role::User, text)
        }
    }

    /// Whether this turn is part of what the model receives.
    fn is_sent(&self) -> bool {
        self.role.is_some() && !self.text.is_empty()
    }

    /// A reply that ran to its end, which makes the turns up to it safe to summarize.
    fn is_completed_reply(&self) -> bool {
        self.role == Some(Role::Assistant)
            && !self.text.trim().is_empty()
            && self.error.is_none()
            && !self.interrupted
    }

    /// The full message content the model receives.
    fn wire_text(&self) -> String {
        if self.context.is_empty() {
            self.text.clone()
        } else {
            format!("{}\n{}", self.context, self.text)
        }
    }

    /// The size of `wire_text` in bytes.
    fn wire_len(&self) -> usize {
        match self.context.len() {
            0 => self.text.len(),
            context => context + 1 + self.text.len(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Waiting,
    Thinking,
    Writing,
    Compacting,
}

/// A `/compact` request on its way back from the model.
#[derive(Debug, Default)]
struct Compaction {
    /// How many turns from the start of the chat the summary will replace.
    replaced: usize,
    reply: String,
    truncated: bool,
    usage: Option<Usage>,
}

#[derive(Debug)]
pub enum Payload {
    Event(StreamEvent),
    Failed(String),
    Ended,
    Title(String),
    ContextWindow(u64),
}

#[derive(Debug)]
pub struct StreamMessage {
    generation: u64,
    payload: Payload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    EditLastReply,
}

/// Progress through the commands matching what was typed before pressing tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub typed: String,
    pub index: usize,
    pub current: String,
}

/// Commands whose names start with `typed`.
pub fn matching_commands(typed: &str) -> Vec<&'static str> {
    COMMANDS.iter().map(|(name, _)| *name).filter(|name| name.starts_with(typed)).collect()
}

fn common_prefix<'a>(names: &[&'a str]) -> &'a str {
    let first = names.first().copied().unwrap_or("");
    let length = names.iter().fold(first.len(), |length, name| {
        first.bytes().zip(name.bytes()).take(length).take_while(|(a, b)| a == b).count()
    });
    &first[..length]
}

pub struct App {
    pub model: String,
    wire_model: String,
    provider: Arc<dyn Provider>,
    resolver: Resolver,
    pub system: Option<String>,
    pub persona: Option<String>,
    pub options: Options,
    pub prefill: Option<String>,
    pub fragments: Vec<String>,
    store: Option<Store>,
    pub conversation_id: Option<String>,
    pub turns: Vec<Turn>,
    pub input: TextArea<'static>,
    pub theme: Theme,
    pub icons: Icons,
    pub animations: bool,
    pub phase: Phase,
    pub frame: u64,
    pub started: Option<Instant>,
    pub show_reasoning: bool,
    pub scroll: usize,
    pub total_usage: Usage,
    /// The server's latest report while it reads the prompt.
    pub prompt_progress: Option<PromptProgress>,
    /// The model's context size in tokens, when the server reports one.
    pub context_window: Option<u64>,
    context_epoch: u64,
    context_task: Option<JoinHandle<()>>,
    /// Tokens the server last counted, and how many turns that count covers.
    measured_context: Option<(u64, usize)>,
    pub notice: Option<String>,
    pub dirty: bool,
    pub should_quit: bool,
    pub action: Option<Action>,
    pub completion: Option<Completion>,
    /// The `/prompts` browser, while it is open.
    pub picker: Option<Picker>,
    pub warn_percent: u8,
    /// The saved default system prompt.
    pub default_prompt: Option<String>,
    default_prompt_file: Option<PathBuf>,
    prompt_pack: Option<PathBuf>,
    exit_armed: Option<Instant>,
    /// Transcript height at the last draw, used to hold the view steady while scrolled up.
    pub last_line_count: usize,
    reply_filter: PrefillFilter,
    pub title: Option<String>,
    pub smart_titles: bool,
    title_settled: bool,
    title_attempts: u8,
    title_epoch: u64,
    title_task: Option<JoinHandle<()>>,
    first_reasoning: Option<Instant>,
    compaction: Option<Compaction>,
    generation: u64,
    task: Option<JoinHandle<()>>,
    sender: UnboundedSender<StreamMessage>,
}

fn new_input(theme: &Theme) -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_cursor_line_style(Style::new());
    input.set_style(Style::new().fg(theme.text));
    input.set_placeholder_text("Message, or / for commands");
    input.set_placeholder_style(Style::new().fg(theme.faint));
    // Long lines wrap so the whole message stays visible; the box grows to fit.
    input.set_wrap_mode(WrapMode::WordOrGlyph);
    input.set_block(
        Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme.faint)),
    );
    input
}

impl App {
    pub fn new(session: ChatSession) -> (Self, UnboundedReceiver<StreamMessage>) {
        let (sender, receiver) = unbounded_channel();
        let mut system = session.system;
        let mut turns = Vec::new();
        let mut parent: Option<String> = None;
        for message in session.history {
            if message.role == Role::System {
                system = system.or(Some(message.content));
                parent = Some(message.id);
                continue;
            }
            let mut turn = if message.compacted_from.is_some() {
                Turn::summary(compact::summary_body(&message.content))
            } else {
                Turn::new(message.role, message.content)
            };
            turn.reasoning = message.reasoning.unwrap_or_default();
            turn.model.clone_from(&session.model.id);
            turn.parent_id = parent.take();
            parent = Some(message.id.clone());
            turn.stored_id = Some(message.id);
            turns.push(turn);
        }
        let default_prompt =
            session.default_prompt_file.as_deref().and_then(library::saved_default);
        let app = Self {
            model: session.model.id,
            wire_model: session.model.wire_name,
            provider: session.model.provider,
            resolver: session.resolver,
            system,
            persona: session.persona,
            options: session.options,
            prefill: session.prefill,
            fragments: session.fragments,
            store: session.store,
            conversation_id: session.conversation_id,
            turns,
            input: new_input(&session.theme),
            theme: session.theme,
            icons: session.icons,
            animations: session.animations,
            phase: Phase::Idle,
            frame: 0,
            started: None,
            show_reasoning: session.show_reasoning,
            scroll: 0,
            total_usage: Usage::default(),
            prompt_progress: None,
            context_window: None,
            context_epoch: 0,
            context_task: None,
            measured_context: None,
            notice: None,
            dirty: true,
            should_quit: false,
            action: None,
            completion: None,
            picker: None,
            warn_percent: session.warn_percent,
            default_prompt,
            default_prompt_file: session.default_prompt_file,
            prompt_pack: session.model.prompt_pack,
            exit_armed: None,
            last_line_count: 0,
            reply_filter: PrefillFilter::default(),
            title_settled: session.title.is_some(),
            title: session.title.map(|title| title::sanitize(&title)),
            smart_titles: session.smart_titles,
            title_attempts: 0,
            title_epoch: 0,
            title_task: None,
            first_reasoning: None,
            compaction: None,
            generation: 0,
            task: None,
            sender,
        };
        (app, receiver)
    }

    pub fn busy(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// Asks the current provider for its context size in the background.
    pub fn request_context_window(&mut self) {
        if let Some(task) = self.context_task.take() {
            task.abort();
        }
        self.context_epoch += 1;
        self.context_window = None;
        let generation = self.context_epoch;
        let provider = Arc::clone(&self.provider);
        let sender = self.sender.clone();
        self.context_task = Some(tokio::spawn(async move {
            if let Some(size) = provider.context_window().await {
                let payload = Payload::ContextWindow(size);
                let _ = sender.send(StreamMessage { generation, payload });
            }
        }));
    }

    /// Tokens the next request will use, and whether that number is an estimate.
    pub fn context_used(&self) -> (u64, bool) {
        if let Some(progress) = self.prompt_progress {
            return (progress.total, false);
        }
        let (measured, counted) = self.measured_context.unwrap_or((0, 0));
        let mut pending: usize = self.turns[counted.min(self.turns.len())..]
            .iter()
            .filter(|turn| turn.role.is_some())
            .map(Turn::wire_len)
            .sum();
        pending += self.fragments.iter().map(String::len).sum::<usize>();
        pending += self.prefill.as_ref().map_or(0, String::len);
        if self.measured_context.is_none() {
            pending += self.system.as_ref().map_or(0, String::len);
        }
        (measured + estimate_tokens(pending), self.measured_context.is_none() || pending > 0)
    }

    /// Estimated tokens for the next request if `prompt_bytes` replaced the system prompt.
    pub fn tokens_with_prompt(&self, prompt_bytes: usize) -> u64 {
        let turns: usize =
            self.turns.iter().filter(|turn| turn.role.is_some()).map(Turn::wire_len).sum();
        let fragments: usize = self.fragments.iter().map(String::len).sum();
        let prefill = self.prefill.as_ref().map_or(0, String::len);
        estimate_tokens(prompt_bytes + turns + fragments + prefill)
    }

    /// A warning when the next request is close to or beyond the model's context window.
    pub fn context_warning(&self) -> Option<(Fit, String)> {
        let window = self.context_window?;
        context::warning(self.context_used().0, window, self.warn_percent)
    }

    pub fn on_terminal(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => match &mut self.picker {
                Some(Picker { path: Some(path), .. }) => path.push_str(text.trim()),
                Some(picker) => picker.push_filter(text.trim()),
                None => {
                    self.input.insert_str(text);
                }
            },
            Event::Resize(..) => {}
            _ => return,
        }
        self.dirty = true;
    }

    fn input_is_empty(&self) -> bool {
        self.input.lines().iter().all(String::is_empty)
    }

    fn press_exit(&mut self, key: &str, exit_pending: bool) {
        if exit_pending {
            self.should_quit = true;
        } else {
            self.exit_armed = Some(Instant::now());
            self.notice = Some(format!("Press {key} again to exit"));
        }
    }

    /// True while the app is waiting for the second exit key press.
    pub fn exit_pending(&self) -> bool {
        self.exit_armed.is_some()
    }

    /// Clears the exit prompt once the time to confirm has passed.
    pub fn expire_exit(&mut self) {
        if self.exit_armed.is_some_and(|armed| armed.elapsed() >= EXIT_WINDOW) {
            self.exit_armed = None;
            self.notice = None;
            self.dirty = true;
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        // Any other key in between cancels a pending exit.
        let exit_pending = self.exit_armed.take().is_some_and(|armed| armed.elapsed() < EXIT_WINDOW);
        self.notice = None;
        if self.picker.is_some() {
            self.on_picker_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('c') if control => {
                if self.busy() {
                    self.cancel();
                } else if self.input_is_empty() {
                    self.press_exit("ctrl+c", exit_pending);
                } else {
                    self.input = new_input(&self.theme);
                }
            }
            KeyCode::Char('d') if control && self.input_is_empty() => {
                self.press_exit("ctrl+d", exit_pending);
            }
            KeyCode::Char('t') if control => {
                self.show_reasoning = !self.show_reasoning;
            }
            KeyCode::Char('j') if control => self.input.insert_newline(),
            KeyCode::BackTab => self.cycle_effort(),
            KeyCode::Tab if self.typing_command().is_some() => self.complete_command(),
            KeyCode::Esc if self.busy() => self.cancel(),
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                self.input.insert_newline();
            }
            KeyCode::Enter => self.submit(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up if self.input_is_empty() => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Down if self.input_is_empty() => self.scroll = self.scroll.saturating_sub(1),
            _ => {
                self.input.input(key);
            }
        }
    }

    /// The command name being typed, when the input is nothing but that name.
    pub fn typing_command(&self) -> Option<&str> {
        let [line] = self.input.lines() else { return None };
        (line.starts_with('/') && !line.contains(char::is_whitespace)).then_some(line.as_str())
    }

    fn set_input(&mut self, text: &str) {
        self.input = new_input(&self.theme);
        self.input.insert_str(text);
    }

    /// Completes a unique match, extends a shared prefix, then cycles through the matches.
    fn complete_command(&mut self) {
        let Some(typed) = self.typing_command().map(str::to_string) else { return };
        let cycling = self.completion.take().filter(|state| state.current == typed);
        let seed = cycling.as_ref().map_or(typed.as_str(), |state| state.typed.as_str());
        let matches = matching_commands(seed);
        match matches.as_slice() {
            [] => self.notice = Some(format!("No command starts with {typed}")),
            [only] => self.set_input(&format!("{only} ")),
            _ => {
                let shared = common_prefix(&matches);
                if cycling.is_none() && shared.len() > typed.len() {
                    self.set_input(shared);
                    return;
                }
                let index = cycling.as_ref().map_or(0, |state| state.index + 1) % matches.len();
                let current = matches[index].to_string();
                self.completion = Some(Completion { typed: seed.to_string(), index, current });
                self.set_input(matches[index]);
            }
        }
    }

    fn cycle_effort(&mut self) {
        let current = self.options.effort.as_deref();
        let index = EFFORTS.iter().position(|effort| *effort == current).unwrap_or(0);
        let next = EFFORTS[(index + 1) % EFFORTS.len()];
        self.options.effort = next.map(str::to_string);
        self.notice = Some(format!("Reasoning effort: {}", next.unwrap_or("auto")));
    }

    fn submit(&mut self) {
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
                help.push("ctrl+t thinking   shift+tab effort   pgup/pgdn scroll".into());
                help.push("tab complete command   ctrl+c or ctrl+d twice to exit".into());
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
    fn clear_unavailable_prompt(&mut self) -> bool {
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

    fn prompts_command(&mut self, argument: &str) {
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
    fn use_prompt(&mut self, name: &str) -> Result<(), String> {
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

    fn apply_prompt(&mut self, loaded: library::Loaded) {
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
    fn set_default_prompt(&mut self, name: &str) -> Result<(), String> {
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

    fn on_picker_key(&mut self, key: KeyEvent) {
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
    fn choose_file(&mut self, path: &str, make_default: bool) -> Result<(), String> {
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

    fn last_reply(&self) -> Option<usize> {
        self.turns.iter().rposition(|turn| turn.role == Some(Role::Assistant))
    }

    fn retry(&mut self) {
        // Notices such as /help may follow the reply without being part of the conversation.
        let latest = self.last_reply().filter(|index| {
            self.turns[index + 1..].iter().all(|turn| turn.role.is_none())
        });
        let Some(index) = latest else {
            self.notice = Some("There is no reply to regenerate".into());
            return;
        };
        self.turns.truncate(index);
        self.measured_context = None;
        self.begin_reply();
    }

    /// The text of the reply `/edit` should open.
    pub fn last_reply_text(&self) -> Option<&str> {
        self.last_reply().map(|index| self.turns[index].text.as_str())
    }

    /// Replaces the last reply. The original stays in history as a sibling branch.
    pub fn apply_edit(&mut self, text: String) {
        let Some(index) = self.last_reply() else { return };
        let conversation = self.conversation_id.clone();
        let turn = &mut self.turns[index];
        // Editors add a final newline, and an emptied file means the edit was abandoned.
        let text = text.trim_end().to_string();
        if text.is_empty() || turn.text.trim_end() == text {
            self.notice = Some("No changes".into());
            return;
        }
        turn.text = text;
        turn.cache = None;
        self.measured_context = None;
        if let (Some(store), Some(conversation)) = (&self.store, conversation) {
            let message = NewMessage {
                model: Some(&turn.model),
                ..NewMessage::plain(turn.parent_id.as_deref(), Role::Assistant, &turn.text)
            };
            match store.append(&conversation, message) {
                Ok(id) => turn.stored_id = Some(id),
                Err(error) => self.notice = Some(format!("Could not save the edit: {error}")),
            }
        }
        self.notice.get_or_insert_with(|| "Reply updated".into());
    }

    fn tip_id(&self) -> Option<String> {
        self.turns.iter().rev().find_map(|turn| turn.stored_id.clone())
    }

    fn send(&mut self, text: String) {
        // A local server may handle one request at a time, so the reply goes first.
        self.stop_title_request();
        self.title.get_or_insert_with(|| title::from_prompt(&text));
        let mut turn = Turn::new(Role::User, text);
        turn.attached_files = self.fragments.len();
        turn.context = std::mem::take(&mut self.fragments).join("\n");
        turn.parent_id = self.tip_id();
        self.persist_user(&mut turn);
        self.turns.push(turn);
        self.begin_reply();
    }

    /// The text for the terminal's title bar.
    pub fn window_title(&self) -> String {
        format!("{} {}", self.icons.assistant, self.title.as_deref().unwrap_or("sparkle"))
    }

    fn stop_title_request(&mut self) {
        if let Some(task) = self.title_task.take() {
            task.abort();
        }
    }

    fn reset_title(&mut self) {
        self.stop_title_request();
        self.title = None;
        self.title_settled = false;
        self.title_attempts = 0;
        self.title_epoch += 1;
    }

    fn apply_generated_title(&mut self, generated: String) {
        self.title_task = None;
        self.title_settled = true;
        self.dirty = true;
        if let (Some(store), Some(conversation)) = (&self.store, &self.conversation_id)
            && let Err(error) = store.set_title(conversation, &generated)
        {
            self.notice = Some(format!("Could not save the title: {error}"));
        }
        self.title = Some(generated);
    }

    /// Names the conversation in the background once a reply has finished.
    fn request_title(&mut self) {
        let replied = self.turns.last().is_some_and(|turn| {
            turn.role == Some(Role::Assistant)
                && turn.error.is_none()
                && !turn.interrupted
                && !turn.text.is_empty()
        });
        if !self.smart_titles || !replied || self.title_settled || self.title_attempts >= 3 {
            return;
        }
        let first = self.turns.iter().find(|turn| turn.role == Some(Role::User) && !turn.summary);
        let Some(prompt) = first.map(|turn| turn.text.clone()) else { return };
        self.title_attempts += 1;

        let request = title::request(&self.wire_model, &prompt);
        let provider = Arc::clone(&self.provider);
        let sender = self.sender.clone();
        let generation = self.title_epoch;
        self.title_task = Some(tokio::spawn(async move {
            let reply = tokio::time::timeout(TITLE_TIMEOUT, async {
                let mut stream = provider.stream(&request).await?;
                let mut reply = String::new();
                while let Some(event) = stream.next().await {
                    if let StreamEvent::Text(text) = event? {
                        reply.push_str(&text);
                    }
                }
                Ok::<String, sparkle_core::Error>(reply)
            })
            .await;
            if let Ok(Ok(reply)) = reply
                && let Some(generated) = title::from_model(&reply)
            {
                let payload = Payload::Title(generated);
                let _ = sender.send(StreamMessage { generation, payload });
            }
        }));
    }

    fn persist_user(&mut self, turn: &mut Turn) {
        let Some(store) = &self.store else { return };
        let result = (|| {
            let conversation = match &self.conversation_id {
                Some(id) => id.clone(),
                None => {
                    let id = store.create_conversation(&turn.text, &self.model)?;
                    if let Some(system) = &self.system {
                        let root = NewMessage::plain(None, Role::System, system);
                        turn.parent_id = Some(store.append(&id, root)?);
                    }
                    self.conversation_id = Some(id.clone());
                    id
                }
            };
            let content = turn.wire_text();
            let message = NewMessage::plain(turn.parent_id.as_deref(), Role::User, &content);
            store.append(&conversation, message)
        })();
        match result {
            Ok(id) => turn.stored_id = Some(id),
            Err(error) => self.notice = Some(format!("Could not save history: {error}")),
        }
    }

    fn request_messages(&self, prefill: Option<&str>) -> Vec<Message> {
        let mut messages = self.wire_messages(&self.turns);
        if let Some(prefill) = prefill {
            messages.push(Message::new(Role::Assistant, prefill));
        }
        messages
    }

    /// The system prompt followed by `turns`, as the model receives them.
    fn wire_messages(&self, turns: &[Turn]) -> Vec<Message> {
        let mut messages = Vec::new();
        if let Some(system) = &self.system {
            messages.push(Message::new(Role::System, system.clone()));
        }
        for turn in turns {
            let Some(role) = turn.role.filter(|_| !turn.text.is_empty()) else { continue };
            match messages.last_mut() {
                // After a failed reply two user messages meet, and strict servers reject that.
                Some(previous) if role == Role::User && previous.role == Role::User => {
                    previous.content.push_str("\n\n");
                    previous.content.push_str(&turn.wire_text());
                }
                _ => messages.push(Message::new(role, turn.wire_text())),
            }
        }
        messages
    }

    /// Asks the model to summarize every turn up to the last completed reply. Anything after that
    /// reply, such as a message that failed to send, is kept word for word.
    fn begin_compaction(&mut self, focus: &str) {
        let Some(last) = self.turns.iter().rposition(Turn::is_completed_reply) else {
            self.notice = Some("Nothing to compact yet. Wait for a finished reply first".into());
            return;
        };
        // A local server may handle one request at a time, so the summary goes first.
        self.stop_title_request();
        let replaced = last + 1;
        let request = compact::request(
            &self.wire_model,
            self.wire_messages(&self.turns[..replaced]),
            self.options.clone(),
            (!focus.is_empty()).then_some(focus),
        );
        self.compaction = Some(Compaction { replaced, ..Compaction::default() });
        self.phase = Phase::Compacting;
        self.start_stream(request);
    }

    fn on_compaction(&mut self, payload: Payload) {
        let Some(compaction) = &mut self.compaction else { return };
        match payload {
            Payload::Event(StreamEvent::PromptProgress(progress)) => {
                if compaction.reply.is_empty() {
                    self.prompt_progress = Some(progress);
                }
            }
            Payload::Event(StreamEvent::Reasoning(_)) => self.prompt_progress = None,
            Payload::Event(StreamEvent::Text(text)) => {
                compaction.reply.push_str(&text);
                self.prompt_progress = None;
            }
            // The server counted the old conversation, so this says nothing about the new one.
            Payload::Event(StreamEvent::Usage(usage)) => {
                compaction.usage = Some(usage);
                self.total_usage.input_tokens += usage.input_tokens;
                self.total_usage.output_tokens += usage.output_tokens;
            }
            Payload::Event(StreamEvent::Finished(reason)) => {
                compaction.truncated = reason == "length";
            }
            Payload::Failed(error) => self.finish_compaction(Err(error)),
            Payload::Ended => self.finish_compaction(Ok(())),
            Payload::Title(_) | Payload::ContextWindow(_) => {}
        }
    }

    /// Returns to idle and hands back the compaction, if one was running.
    fn stop_compaction(&mut self) -> Option<Compaction> {
        self.compaction.as_ref()?;
        self.phase = Phase::Idle;
        self.started = None;
        self.prompt_progress = None;
        self.task = None;
        self.dirty = true;
        self.compaction.take()
    }

    fn finish_compaction(&mut self, outcome: std::result::Result<(), String>) {
        let Some(compaction) = self.stop_compaction() else { return };
        let applied = outcome.and_then(|()| self.apply_compaction(compaction));
        if let Err(reason) = applied {
            let text = format!("Compaction failed, so the conversation is unchanged: {reason}");
            self.turns.push(Turn::notice(text));
        }
    }

    /// Swaps the summarized turns for the summary, unless that would lose something.
    fn apply_compaction(&mut self, compaction: Compaction) -> std::result::Result<(), String> {
        if compaction.truncated {
            return Err("the summary was cut off by the token limit".into());
        }
        let text = compact::summary_from_reply(&compaction.reply)
            .ok_or("the model returned an empty summary")?;
        let replaced = compaction.replaced;
        let sent = || self.turns[..replaced].iter().filter(|turn| turn.is_sent());
        let (count, before) = (sent().count(), sent().map(Turn::wire_len).sum::<usize>());

        let mut summary = Turn::summary(text);
        if summary.wire_len() >= before {
            return Err("the summary was no shorter than the messages it would replace".into());
        }
        summary.model.clone_from(&self.model);
        summary.usage = compaction.usage;
        // The summary hangs from the stored system prompt, which stays at the root of the thread.
        summary.parent_id =
            self.turns.iter().find(|turn| turn.role.is_some()).and_then(|turn| turn.parent_id.clone());
        let saved = self
            .save_compaction(&summary, replaced)
            .map_err(|error| format!("could not save the summary: {error}"))?;

        let after = summary.wire_len();
        let notice = Turn::notice(format!(
            "Compacted {count} message{} (~{} -> ~{} tokens)",
            if count == 1 { "" } else { "s" },
            compact_count(estimate_tokens(before)),
            compact_count(estimate_tokens(after)),
        ));
        if let Some((summary_id, kept_ids)) = saved {
            summary.stored_id = Some(summary_id.clone());
            self.relink_kept_turns(replaced, summary_id, kept_ids);
        }
        drop(self.turns.splice(..replaced, [summary, notice]));
        self.measured_context = None;
        self.scroll = 0;
        Ok(())
    }

    /// Stores the summary and copies of the turns kept after it, when this chat is being saved.
    fn save_compaction(
        &self,
        summary: &Turn,
        replaced: usize,
    ) -> sparkle_store::Result<Option<(String, Vec<String>)>> {
        let (Some(store), Some(conversation), Some(tip)) =
            (&self.store, &self.conversation_id, self.tip_id())
        else {
            return Ok(None);
        };
        let content = summary.wire_text();
        let message = NewMessage {
            model: Some(&summary.model),
            usage: summary.usage,
            ..NewMessage::plain(summary.parent_id.as_deref(), Role::User, &content)
        };
        let kept: Vec<(&Turn, String)> = self.turns[replaced..]
            .iter()
            .filter(|turn| turn.is_sent())
            .map(|turn| (turn, turn.wire_text()))
            .collect();
        let copies: Vec<NewMessage<'_>> = kept
            .iter()
            .filter_map(|(turn, content)| {
                Some(NewMessage {
                    parent_id: None,
                    role: turn.role?,
                    content,
                    reasoning: (!turn.reasoning.is_empty()).then_some(turn.reasoning.as_str()),
                    model: (!turn.model.is_empty()).then_some(turn.model.as_str()),
                    usage: turn.usage,
                    duration_ms: turn.duration_ms,
                })
            })
            .collect();
        store.compact(conversation, message, &tip, &copies).map(Some)
    }

    /// Points the turns after the summary at their copies on the new branch.
    fn relink_kept_turns(&mut self, replaced: usize, summary_id: String, kept_ids: Vec<String>) {
        let mut parent = summary_id;
        let mut ids = kept_ids.into_iter();
        for turn in self.turns[replaced..].iter_mut().filter(|turn| turn.role.is_some()) {
            turn.parent_id = Some(parent.clone());
            turn.stored_id = if turn.is_sent() { ids.next() } else { None };
            if let Some(id) = &turn.stored_id {
                parent.clone_from(id);
            }
        }
    }

    fn begin_reply(&mut self) {
        let prefill = self.prefill.take();
        self.reply_filter = PrefillFilter::new(prefill.as_deref().unwrap_or(""));
        let request = ChatRequest {
            model: self.wire_model.clone(),
            messages: self.request_messages(prefill.as_deref()),
            options: self.options.clone(),
            schema: None,
            stream: true,
        };
        let mut turn = Turn::new(Role::Assistant, prefill.unwrap_or_default());
        turn.model.clone_from(&self.model);
        turn.parent_id = self.tip_id();
        self.turns.push(turn);
        self.phase = Phase::Waiting;
        self.start_stream(request);
    }

    /// Sends a request in the background, with its events arriving through `on_stream`.
    fn start_stream(&mut self, request: ChatRequest) {
        self.started = Some(Instant::now());
        self.first_reasoning = None;
        self.prompt_progress = None;
        self.generation += 1;

        let generation = self.generation;
        let sender = self.sender.clone();
        let provider = Arc::clone(&self.provider);
        self.task = Some(tokio::spawn(async move {
            let result = async {
                let mut stream = provider.stream(&request).await?;
                while let Some(event) = stream.next().await {
                    let payload = Payload::Event(event?);
                    if sender.send(StreamMessage { generation, payload }).is_err() {
                        break;
                    }
                }
                Ok::<(), sparkle_core::Error>(())
            }
            .await;
            let payload = match result {
                Ok(()) => Payload::Ended,
                Err(error) => Payload::Failed(error.to_string()),
            };
            let _ = sender.send(StreamMessage { generation, payload });
        }));
    }

    pub fn on_stream(&mut self, message: StreamMessage) {
        if let Payload::Title(title) = &message.payload {
            if message.generation == self.title_epoch {
                self.apply_generated_title(title.clone());
            }
            return;
        }
        if let Payload::ContextWindow(size) = message.payload {
            if message.generation == self.context_epoch {
                self.context_window = Some(size);
                self.dirty = true;
            }
            return;
        }
        if message.generation != self.generation || !self.busy() {
            return;
        }
        self.dirty = true;
        if self.phase == Phase::Compacting {
            self.on_compaction(message.payload);
            return;
        }
        match message.payload {
            Payload::Event(event) => self.on_event(event),
            Payload::Failed(error) => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.error = Some(error);
                }
                self.finish();
            }
            Payload::Ended => self.finish(),
            Payload::Title(_) | Payload::ContextWindow(_) => {}
        }
    }

    fn elapsed_ms(since: Option<Instant>) -> Option<u64> {
        since.map(|start| start.elapsed().as_millis() as u64)
    }

    fn on_event(&mut self, event: StreamEvent) {
        // Progress changes nothing in the transcript, so its rendered lines stay cached.
        if let StreamEvent::PromptProgress(progress) = event {
            if self.phase == Phase::Waiting {
                self.prompt_progress = Some(progress);
            }
            return;
        }
        let thought = Self::elapsed_ms(self.first_reasoning);
        let counted = self.turns.len();
        let Some(turn) = self.turns.last_mut() else { return };
        turn.cache = None;
        match event {
            StreamEvent::PromptProgress(_) => {}
            StreamEvent::Reasoning(text) => {
                turn.reasoning.push_str(&text);
                self.first_reasoning.get_or_insert_with(Instant::now);
                self.phase = Phase::Thinking;
                self.prompt_progress = None;
            }
            StreamEvent::Text(text) => {
                turn.text.push_str(&self.reply_filter.push(&text));
                turn.thought_ms = turn.thought_ms.or(thought);
                self.phase = Phase::Writing;
                self.prompt_progress = None;
            }
            StreamEvent::Usage(usage) => {
                turn.usage = Some(usage);
                self.total_usage.input_tokens += usage.input_tokens;
                self.total_usage.output_tokens += usage.output_tokens;
                if usage.input_tokens > 0 {
                    let tokens = usage.input_tokens + usage.output_tokens;
                    self.measured_context = Some((tokens, counted));
                }
            }
            StreamEvent::Finished(_) => {}
        }
    }

    /// Stops the reply in progress and keeps what has arrived so far.
    pub fn cancel(&mut self) {
        if !self.busy() {
            return;
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if self.stop_compaction().is_some() {
            self.notice = Some("Compaction stopped. The conversation is unchanged".into());
            return;
        }
        if let Some(turn) = self.turns.last_mut() {
            turn.interrupted = true;
        }
        self.finish();
    }

    fn finish(&mut self) {
        let duration = Self::elapsed_ms(self.started);
        let thought = Self::elapsed_ms(self.first_reasoning);
        self.phase = Phase::Idle;
        self.started = None;
        self.prompt_progress = None;
        self.task = None;
        self.dirty = true;
        let conversation = self.conversation_id.clone();
        let Some(turn) = self.turns.last_mut() else { return };
        turn.text.push_str(&self.reply_filter.finish());
        turn.cache = None;
        turn.duration_ms = duration;
        if !turn.reasoning.is_empty() {
            turn.thought_ms = turn.thought_ms.or(thought);
        }
        // A reply stopped before any text arrived has nothing worth resending later.
        let saved = !turn.text.is_empty();
        if let (Some(store), Some(conversation), true) = (&self.store, conversation, saved) {
            let message = NewMessage {
                parent_id: turn.parent_id.as_deref(),
                role: Role::Assistant,
                content: &turn.text,
                reasoning: (!turn.reasoning.is_empty()).then_some(turn.reasoning.as_str()),
                model: Some(&turn.model),
                usage: turn.usage,
                duration_ms: duration,
            };
            match store.append(&conversation, message) {
                Ok(id) => turn.stored_id = Some(id),
                Err(error) => self.notice = Some(format!("Could not save history: {error}")),
            }
        }
        self.request_title();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures_util::future::BoxFuture;
    use futures_util::stream;
    use sparkle_core::{EventStream, Result};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    pub(crate) struct FakeProvider {
        pub events: Vec<StreamEvent>,
        /// Replies for the next requests, used ahead of `events`.
        pub queued: Mutex<VecDeque<Vec<StreamEvent>>>,
        pub requests: Mutex<Vec<ChatRequest>>,
    }

    impl FakeProvider {
        pub(crate) fn queue(&self, events: Vec<StreamEvent>) {
            self.queued.lock().unwrap().push_back(events);
        }
    }

    impl Provider for FakeProvider {
        fn stream<'a>(&'a self, request: &'a ChatRequest) -> BoxFuture<'a, Result<EventStream>> {
            self.requests.lock().unwrap().push(request.clone());
            let queued = self.queued.lock().unwrap().pop_front();
            let events = queued.unwrap_or_else(|| self.events.clone());
            Box::pin(async move {
                Ok(Box::pin(stream::iter(events.into_iter().map(Ok))) as EventStream)
            })
        }
    }

    pub(crate) fn reply_events() -> Vec<StreamEvent> {
        vec![
            StreamEvent::Reasoning("pondering".into()),
            StreamEvent::Text("Hello **there**".into()),
            StreamEvent::Usage(Usage { input_tokens: 5, output_tokens: 3 }),
            StreamEvent::Finished("stop".into()),
        ]
    }

    pub(crate) fn app_with(
        events: Vec<StreamEvent>,
        store: Option<Store>,
    ) -> (App, UnboundedReceiver<StreamMessage>, Arc<FakeProvider>) {
        let provider = Arc::new(FakeProvider {
            events,
            queued: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        });
        let (app, receiver) = App::new(session_with(provider.clone(), store, None, Vec::new()));
        (app, receiver, provider)
    }

    fn session_with(
        provider: Arc<FakeProvider>,
        store: Option<Store>,
        conversation_id: Option<String>,
        history: Vec<StoredMessage>,
    ) -> ChatSession {
        ChatSession {
            model: ResolvedModel {
                id: "mock".into(),
                wire_name: "mock-wire".into(),
                provider,
                prompt_pack: None,
            },
            resolver: Box::new(|name| Err(format!("unknown model {name}"))),
            system: None,
            persona: None,
            options: Options::default(),
            prefill: None,
            fragments: Vec::new(),
            store,
            conversation_id,
            history,
            theme: Theme::DUSK,
            icons: Icons::ASCII,
            animations: true,
            show_reasoning: true,
            title: None,
            smart_titles: false,
            warn_percent: 80,
            default_prompt_file: None,
        }
    }

    pub(crate) async fn drain(app: &mut App, receiver: &mut UnboundedReceiver<StreamMessage>) {
        while app.busy() {
            let message = receiver.recv().await.expect("the stream stays open");
            app.on_stream(message);
        }
    }

    pub(crate) fn type_text(app: &mut App, text: &str) {
        app.input.insert_str(text);
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.on_terminal(Event::Key(KeyEvent::new(code, modifiers)));
    }

    fn tab(app: &mut App) -> String {
        press(app, KeyCode::Tab, KeyModifiers::NONE);
        app.input.lines().join("\n")
    }

    fn default_file(name: &str) -> PathBuf {
        let unique = format!("sparkle-app-{}-{name}", std::process::id());
        let file = std::env::temp_dir().join(unique).join("default_prompt.txt");
        let _ = std::fs::remove_file(&file);
        file
    }

    #[tokio::test]
    async fn a_red_team_prompt_is_cleared_when_its_pack_is_unavailable() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.persona = Some("redteam/example/prompt".into());
        app.system = Some("text".into());
        assert!(app.clear_unavailable_prompt());
        assert_eq!((app.persona.as_deref(), app.system.as_deref()), (None, None));

        app.persona = Some("openai/gpt-3.5".into());
        app.system = Some("text".into());
        assert!(!app.clear_unavailable_prompt());
        assert_eq!(app.system.as_deref(), Some("text"));
    }

    #[tokio::test]
    async fn choosing_a_prompt_replaces_the_system_prompt_and_its_measurement() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.measured_context = Some((500, 0));
        app.use_prompt("openai/gpt-3.5").unwrap();
        assert_eq!(app.persona.as_deref(), Some("openai/gpt-3.5"));
        assert!(app.system.as_deref().unwrap().contains("GPT-3.5"));
        assert_eq!(app.measured_context, None);

        app.use_prompt("off").unwrap();
        assert_eq!((app.persona.as_deref(), app.system.as_deref()), (None, None));
    }

    #[tokio::test]
    async fn the_browser_keeps_keys_away_from_the_message_box() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.prompts_command("");
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(app.picker.as_ref().unwrap().filter, "x");
        assert!(app.input_is_empty());
        app.on_terminal(Event::Paste("grok".into()));
        assert_eq!(app.picker.as_ref().unwrap().filter, "xgrok");

        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.picker.is_none());
        assert!(!app.exit_pending(), "closing the browser does not arm exit");
    }

    #[tokio::test]
    async fn ctrl_d_toggles_the_highlighted_prompt_as_the_default() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        let file = default_file("toggle");
        app.default_prompt_file = Some(file.clone());
        app.prompts_command("");
        app.picker.as_mut().unwrap().push_filter("gpt-3.5");

        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(library::saved_default(&file).as_deref(), Some("openai/gpt-3.5"));
        assert!(app.picker.is_some(), "the browser stays open");
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(library::saved_default(&file), None);
        assert_eq!(app.default_prompt, None);
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_read_keeps_the_browser_open() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.prompts_command("file");
        app.on_terminal(Event::Paste("/nonexistent/prompt.md".into()));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let error = app.picker.as_ref().unwrap().error.clone().unwrap();
        assert!(error.contains("could not read"), "{error}");
        assert_eq!(app.system, None);

        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.picker.as_ref().unwrap().path.is_none(), "esc returns to the list");
    }

    #[tokio::test]
    async fn a_file_named_like_a_persona_is_still_read_as_a_file() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        let directory = default_file("persona-name").parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("concise");
        std::fs::write(&path, "From the file").unwrap();
        app.choose_file(path.to_str().unwrap(), false).unwrap();
        assert_eq!(app.system.as_deref(), Some("From the file"));
    }

    #[tokio::test]
    async fn defaults_cannot_be_saved_without_a_place_to_keep_them() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.prompts_command("default gpt-3.5");
        assert!(app.notice.as_deref().unwrap().contains("cannot be saved"));
    }

    #[tokio::test]
    async fn tab_completes_a_unique_command_ready_for_its_argument() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "/mo");
        assert_eq!(tab(&mut app), "/model ");
    }

    #[tokio::test]
    async fn tab_cycles_through_ambiguous_commands_and_wraps() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "/e");
        assert_eq!(tab(&mut app), "/effort");
        assert_eq!(tab(&mut app), "/edit");
        assert_eq!(tab(&mut app), "/effort");
    }

    #[tokio::test]
    async fn tab_extends_a_shared_prefix_before_cycling() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "/pe");
        assert_eq!(tab(&mut app), "/persona ");
        assert_eq!(common_prefix(&["/retry", "/rewind"]), "/re");
    }

    #[tokio::test]
    async fn editing_after_tab_starts_a_new_completion() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "/e");
        assert_eq!(tab(&mut app), "/effort");
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.input.lines().join(""), "/ef");
        assert_eq!(tab(&mut app), "/effort ");
    }

    #[tokio::test]
    async fn tab_leaves_ordinary_text_and_arguments_alone() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "/model gp");
        assert!(tab(&mut app).starts_with("/model gp"));
        assert!(app.completion.is_none());

        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "/zzz");
        assert_eq!(tab(&mut app), "/zzz");
        assert!(app.notice.as_deref().unwrap().contains("No command"));
    }

    #[tokio::test]
    async fn fragments_reach_the_model_with_the_first_message_only() {
        let store = Store::in_memory().unwrap();
        let (mut app, mut receiver, provider) = app_with(reply_events(), Some(store));
        app.fragments = vec!["FILE\n".into()];
        type_text(&mut app, "first");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        type_text(&mut app, "second");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;

        assert_eq!(app.turns[0].text, "first");
        assert!(app.fragments.is_empty());
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[0].messages[0].content, "FILE\n\nfirst");
        let last = &requests[1].messages;
        assert_eq!(last[0].content, "FILE\n\nfirst");
        assert_eq!(last[2].content, "second");
        let store = app.store.as_ref().unwrap();
        let thread = store.thread(app.conversation_id.as_ref().unwrap()).unwrap();
        assert_eq!(thread[0].content, "FILE\n\nfirst");
    }

    #[tokio::test]
    async fn sends_a_message_and_collects_the_reply() {
        let (mut app, mut receiver, provider) = app_with(reply_events(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;

        let reply = app.turns.last().unwrap();
        assert_eq!(reply.text, "Hello **there**");
        assert_eq!(reply.reasoning, "pondering");
        assert_eq!(app.total_usage.output_tokens, 3);
        assert_eq!(provider.requests.lock().unwrap()[0].model, "mock-wire");
    }

    #[tokio::test]
    async fn prefill_is_sent_and_starts_the_reply() {
        let (mut app, mut receiver, provider) =
            app_with(vec![StreamEvent::Text(" world".into())], None);
        type_text(&mut app, "/prefill Hello");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut app, "greet me");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;

        assert_eq!(app.turns.last().unwrap().text, "Hello world");
        assert!(app.prefill.is_none());
        let requests = provider.requests.lock().unwrap();
        let last = requests[0].messages.last().unwrap();
        assert_eq!((last.role, last.content.as_str()), (Role::Assistant, "Hello"));
    }

    /// Waits for the background title request to report back.
    async fn drain_title(app: &mut App, receiver: &mut UnboundedReceiver<StreamMessage>) {
        let message = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("a title arrives")
            .expect("the channel stays open");
        app.on_stream(message);
    }

    #[tokio::test]
    async fn the_title_starts_from_the_prompt_and_the_icon_comes_first() {
        let (mut app, mut receiver, provider) = app_with(reply_events(), None);
        app.icons = Icons::NERD;
        assert_eq!(app.window_title(), "\u{f0674} sparkle");
        type_text(&mut app, "Why is my parser slow?\nDetails follow");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.window_title(), "\u{f0674} Why is my parser slow?");
        drain(&mut app, &mut receiver).await;
        assert_eq!(provider.requests.lock().unwrap().len(), 1, "smart titles are off");
    }

    #[tokio::test]
    async fn the_model_names_the_chat_after_the_first_reply() {
        let store = Store::in_memory().unwrap();
        let events = vec![StreamEvent::Text("\"Parser speedups.\"".into())];
        let (mut app, mut receiver, provider) = app_with(events, Some(store));
        app.smart_titles = true;
        type_text(&mut app, "Why is my parser slow?");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        drain_title(&mut app, &mut receiver).await;

        assert_eq!(app.title.as_deref(), Some("Parser speedups"));
        let store = app.store.as_ref().unwrap();
        let conversation = app.conversation_id.as_ref().unwrap();
        assert_eq!(store.conversation_title(conversation).unwrap().unwrap(), "Parser speedups");

        type_text(&mut app, "next");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "the title is only requested once");
        assert_eq!(requests[1].messages[1].content, "Why is my parser slow?");
        assert_eq!(requests[1].messages[0].role, Role::System);
    }

    #[tokio::test]
    async fn a_failed_or_stopped_reply_does_not_request_a_title() {
        let (mut app, _receiver, provider) = app_with(Vec::new(), None);
        app.smart_titles = true;
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let generation = app.generation;
        app.on_stream(StreamMessage { generation, payload: Payload::Failed("boom".into()) });
        assert!(app.title_task.is_none());

        type_text(&mut app, "again");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.title_task.is_none());
        drop(provider);
    }

    #[tokio::test]
    async fn a_new_chat_discards_a_late_title() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "first");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.cancel();
        let stale = app.title_epoch;
        type_text(&mut app, "/new");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.title, None);
        app.on_stream(StreamMessage { generation: stale, payload: Payload::Title("Old".into()) });
        assert_eq!(app.title, None);
    }

    #[tokio::test]
    async fn a_resumed_chat_keeps_its_saved_title() {
        let (mut app, mut receiver, provider) = app_with(reply_events(), None);
        app.smart_titles = true;
        app.title = Some("Saved".into());
        app.title_settled = true;
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        assert_eq!(app.title.as_deref(), Some("Saved"));
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_server_that_restarts_after_a_prefill_does_not_duplicate_it() {
        let events = vec![StreamEvent::Text("The answer".into()), StreamEvent::Text(" is 4.".into())];
        let (mut app, mut receiver, _provider) = app_with(events, None);
        type_text(&mut app, "/prefill The answer is");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut app, "2+2?");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        assert_eq!(app.turns.last().unwrap().text, "The answer is 4.");
    }

    #[tokio::test]
    async fn a_path_at_the_start_of_a_message_is_not_a_command() {
        let (mut app, mut receiver, provider) = app_with(reply_events(), None);
        type_text(&mut app, "/etc/hosts looks wrong");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        assert_eq!(provider.requests.lock().unwrap()[0].messages[0].content, "/etc/hosts looks wrong");
    }

    #[tokio::test]
    async fn an_unknown_command_keeps_what_was_typed() {
        let (mut app, _receiver, provider) = app_with(reply_events(), None);
        type_text(&mut app, "/modle gpt");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.input.lines().join(""), "/modle gpt");
        assert!(app.notice.as_deref().unwrap().contains("Unknown command /modle"));
        assert!(provider.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn retry_works_after_a_notice_follows_the_reply() {
        let (mut app, mut receiver, provider) = app_with(reply_events(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        type_text(&mut app, "/help");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut app, "/retry");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        assert_eq!(app.turns.len(), 2);
    }

    #[tokio::test]
    async fn messages_after_a_failed_reply_are_merged_for_strict_servers() {
        let (mut app, _receiver, provider) = app_with(Vec::new(), None);
        type_text(&mut app, "first");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let generation = app.generation;
        app.on_stream(StreamMessage { generation, payload: Payload::Failed("boom".into()) });
        type_text(&mut app, "second");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        tokio::task::yield_now().await;
        let messages = app.request_messages(None);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "first\n\nsecond");
        drop(provider);
    }

    #[tokio::test]
    async fn an_editor_newline_or_emptied_file_is_not_an_edit() {
        let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        app.apply_edit("Hello **there**\n".into());
        assert_eq!(app.notice.as_deref(), Some("No changes"));
        app.apply_edit("\n".into());
        assert_eq!(app.turns.last().unwrap().text, "Hello **there**");
        app.apply_edit("Better\n".into());
        assert_eq!(app.turns.last().unwrap().text, "Better");
    }

    #[tokio::test]
    async fn retry_replaces_the_reply_and_keeps_both_in_history() {
        let store = Store::in_memory().unwrap();
        let (mut app, mut receiver, provider) = app_with(reply_events(), Some(store));
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        type_text(&mut app, "/retry");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;

        assert_eq!(app.turns.len(), 2);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[1].messages.len(), 1);
        let store = app.store.as_ref().unwrap();
        let conversation = app.conversation_id.as_ref().unwrap();
        assert_eq!(store.thread(conversation).unwrap().len(), 2);
        assert_eq!(store.recent(1).unwrap()[0].messages, 3);
    }

    #[tokio::test]
    async fn edited_reply_is_used_in_the_next_request() {
        let store = Store::in_memory().unwrap();
        let (mut app, mut receiver, provider) = app_with(reply_events(), Some(store));
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        app.apply_edit("Edited".into());
        type_text(&mut app, "next");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[1].messages[1].content, "Edited");
        let store = app.store.as_ref().unwrap();
        let thread = store.thread(app.conversation_id.as_ref().unwrap()).unwrap();
        let contents: Vec<_> = thread.iter().map(|message| message.content.as_str()).collect();
        assert_eq!(contents, ["hi", "Edited", "next", "Hello **there**"]);
    }

    #[tokio::test]
    async fn failures_stay_in_the_chat() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let generation = app.generation;
        app.on_stream(StreamMessage { generation, payload: Payload::Failed("boom".into()) });
        assert_eq!(app.turns.last().unwrap().error.as_deref(), Some("boom"));
        assert!(!app.busy());
        assert!(!app.should_quit);
    }

    #[tokio::test]
    async fn escape_stops_and_ignores_late_events() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let generation = app.generation;
        app.on_stream(StreamMessage {
            generation,
            payload: Payload::Event(StreamEvent::Text("partial".into())),
        });
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.on_stream(StreamMessage {
            generation,
            payload: Payload::Event(StreamEvent::Text(" late".into())),
        });
        let turn = app.turns.last().unwrap();
        assert_eq!(turn.text, "partial");
        assert!(turn.interrupted);
    }

    fn progress(processed: u64, total: u64) -> StreamEvent {
        StreamEvent::PromptProgress(PromptProgress { processed, total, cache: 0, time_ms: 1 })
    }

    fn deliver(app: &mut App, event: StreamEvent) {
        let generation = app.generation;
        app.on_stream(StreamMessage { generation, payload: Payload::Event(event) });
    }

    #[tokio::test]
    async fn prefill_progress_lasts_until_the_first_token() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        deliver(&mut app, progress(10, 100));
        deliver(&mut app, progress(50, 100));
        assert_eq!(app.prompt_progress.unwrap().processed, 50);
        assert_eq!(app.phase, Phase::Waiting);

        deliver(&mut app, StreamEvent::Text("hello".into()));
        assert_eq!(app.prompt_progress, None);
        deliver(&mut app, progress(100, 100));
        assert_eq!(app.prompt_progress, None, "late reports are ignored");
    }

    #[tokio::test]
    async fn stopping_clears_prefill_progress() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        deliver(&mut app, progress(10, 100));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.prompt_progress, None);
    }

    #[tokio::test]
    async fn prefill_progress_keeps_the_rendered_transcript() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.turns.last_mut().unwrap().cache = Some((80, true, Vec::new()));
        deliver(&mut app, progress(10, 100));
        assert!(app.turns.last().unwrap().cache.is_some());
    }

    #[tokio::test]
    async fn a_context_size_from_a_previous_model_is_discarded() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        app.request_context_window();
        let stale = app.context_epoch;
        app.request_context_window();
        let current = app.context_epoch;
        app.on_stream(StreamMessage { generation: stale, payload: Payload::ContextWindow(1) });
        assert_eq!(app.context_window, None);
        app.on_stream(StreamMessage { generation: current, payload: Payload::ContextWindow(2) });
        assert_eq!(app.context_window, Some(2));
    }

    #[tokio::test]
    async fn context_is_estimated_until_the_server_counts_it() {
        let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
        app.system = Some("s".repeat(400));
        app.fragments = vec!["f".repeat(40)];
        assert_eq!(app.context_used(), (110, true));

        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        assert_eq!(app.context_used(), (8, false), "usage replaces the estimate");

        app.prefill = Some("p".repeat(8));
        assert_eq!(app.context_used(), (10, true), "unsent text is added to the count");
    }

    #[tokio::test]
    async fn changing_the_conversation_drops_the_server_count() {
        let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        drain(&mut app, &mut receiver).await;
        assert!(!app.context_used().1);

        type_text(&mut app, "/system Be brief");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.context_used().1);
    }

    #[tokio::test]
    async fn prefill_reports_the_exact_prompt_size() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        deliver(&mut app, progress(10, 5000));
        assert_eq!(app.context_used(), (5000, false));
    }

    #[tokio::test]
    async fn shift_tab_cycles_effort() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        press(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(app.options.effort.as_deref(), Some("low"));
        for _ in 0..3 {
            press(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        }
        assert_eq!(app.options.effort, None);
    }

    #[tokio::test]
    async fn ctrl_c_clears_input_then_needs_two_presses_to_exit() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "draft");
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.input_is_empty());
        assert!(!app.exit_pending());

        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.should_quit);
        assert_eq!(app.notice.as_deref(), Some("Press ctrl+c again to exit"));
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn ctrl_d_needs_two_presses_to_exit() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(!app.should_quit);
        assert_eq!(app.notice.as_deref(), Some("Press ctrl+d again to exit"));
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn ctrl_d_with_text_in_the_input_does_not_start_an_exit() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "draft");
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(!app.should_quit);
        assert!(!app.exit_pending());
    }

    #[tokio::test]
    async fn another_key_cancels_a_pending_exit() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert!(!app.exit_pending());
        assert_eq!(app.notice, None);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.should_quit);
    }

    #[tokio::test]
    async fn pending_exit_expires() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.exit_armed = Instant::now().checked_sub(EXIT_WINDOW);
        app.expire_exit();
        assert!(!app.exit_pending());
        assert_eq!(app.notice, None);

        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.exit_armed = Instant::now().checked_sub(EXIT_WINDOW);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.should_quit, "a late second press only starts a new exit");
        assert!(app.exit_pending());
    }

    // Compaction

    const SUMMARY: &str = "They settled on SQLite.";

    fn long_reply() -> Vec<StreamEvent> {
        vec![
            StreamEvent::Text("a long and detailed answer ".repeat(30)),
            StreamEvent::Usage(Usage { input_tokens: 500, output_tokens: 200 }),
            StreamEvent::Finished("stop".into()),
        ]
    }

    fn summary_reply() -> Vec<StreamEvent> {
        vec![
            StreamEvent::Reasoning("what matters here".into()),
            StreamEvent::Text(SUMMARY.into()),
            StreamEvent::Usage(Usage { input_tokens: 700, output_tokens: 10 }),
            StreamEvent::Finished("stop".into()),
        ]
    }

    async fn say(app: &mut App, receiver: &mut UnboundedReceiver<StreamMessage>, text: &str) {
        type_text(app, text);
        press(app, KeyCode::Enter, KeyModifiers::NONE);
        drain(app, receiver).await;
    }

    /// A chat with two finished exchanges and a summary waiting to be requested.
    async fn compactable(
        store: Option<Store>,
    ) -> (App, UnboundedReceiver<StreamMessage>, Arc<FakeProvider>) {
        let (mut app, mut receiver, provider) = app_with(long_reply(), store);
        app.system = Some("Be brief".into());
        say(&mut app, &mut receiver, "first").await;
        say(&mut app, &mut receiver, "second").await;
        provider.queue(summary_reply());
        (app, receiver, provider)
    }

    fn roles_and_text(app: &App) -> Vec<(Option<Role>, String)> {
        app.turns.iter().map(|turn| (turn.role, turn.text.clone())).collect()
    }

    #[tokio::test]
    async fn compact_is_offered_as_a_command() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "/co");
        assert_eq!(tab(&mut app), "/compact ");
        assert!(COMMANDS.contains(&("/compact", "Summarize earlier messages to free context")));
    }

    #[tokio::test]
    async fn an_empty_chat_has_nothing_to_compact() {
        let (mut app, mut receiver, provider) = app_with(reply_events(), None);
        say(&mut app, &mut receiver, "/compact").await;
        assert!(app.notice.as_deref().unwrap().contains("Nothing to compact"));
        assert!(app.turns.is_empty());
        assert!(provider.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_chat_without_a_finished_reply_has_nothing_to_compact() {
        let (mut app, _receiver, provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        deliver(&mut app, StreamEvent::Text("partial".into()));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let sent = provider.requests.lock().unwrap().len();

        type_text(&mut app, "/compact");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.notice.as_deref().unwrap().contains("Nothing to compact"));
        assert!(!app.busy());
        assert_eq!(app.turns.len(), 2);
        assert_eq!(provider.requests.lock().unwrap().len(), sent);
    }

    #[tokio::test]
    async fn a_fresh_summary_has_nothing_more_to_compact() {
        let (mut app, mut receiver, provider) = compactable(None).await;
        say(&mut app, &mut receiver, "/compact").await;
        let sent = provider.requests.lock().unwrap().len();
        say(&mut app, &mut receiver, "/compact").await;
        assert!(app.notice.as_deref().unwrap().contains("Nothing to compact"));
        assert_eq!(provider.requests.lock().unwrap().len(), sent);
    }

    #[tokio::test]
    async fn compaction_is_refused_while_a_reply_is_streaming() {
        let (mut app, _receiver, provider) = app_with(long_reply(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut app, "/compact");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.notice.as_deref(), Some("Still replying. Press esc to stop."));
        assert_eq!(app.input.lines().join(""), "/compact", "the command is not lost");
        assert_eq!(app.phase, Phase::Waiting);
        tokio::task::yield_now().await;
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn other_input_waits_while_compacting() {
        let (mut app, _receiver, _provider) = compactable(None).await;
        type_text(&mut app, "/compact");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.phase, Phase::Compacting);
        type_text(&mut app, "next question");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.notice.as_deref(), Some("Still compacting. Press esc to stop."));
        assert_eq!(app.input.lines().join(""), "next question");
    }

    #[tokio::test]
    async fn the_summary_request_carries_the_system_prompt_and_every_turn() {
        let (mut app, mut receiver, provider) = compactable(None).await;
        say(&mut app, &mut receiver, "/compact").await;

        let requests = provider.requests.lock().unwrap();
        let messages = &requests[2].messages;
        assert_eq!(requests[2].model, "mock-wire");
        assert_eq!(messages.len(), 6);
        assert_eq!((messages[0].role, messages[0].content.as_str()), (Role::System, "Be brief"));
        assert_eq!(messages[1].content, "first");
        assert_eq!(messages[3].content, "second");
        assert_eq!(messages[5].role, Role::User);
        assert!(messages[5].content.starts_with("Summarize the conversation"));
        assert!(!messages[5].content.contains("focus on"));
    }

    #[tokio::test]
    async fn focus_text_reaches_the_summary_request() {
        let (mut app, mut receiver, provider) = compactable(None).await;
        say(&mut app, &mut receiver, "/compact   keep all SQL schemas  ").await;
        let requests = provider.requests.lock().unwrap();
        let instructions = &requests[2].messages.last().unwrap().content;
        assert!(instructions.ends_with("focus on: keep all SQL schemas"), "{instructions}");
    }

    #[tokio::test]
    async fn compaction_shrinks_what_the_next_request_sends() {
        let (mut app, mut receiver, provider) = compactable(None).await;
        let before = app.request_messages(None);
        let (context_before, _) = app.context_used();
        say(&mut app, &mut receiver, "/compact").await;

        let after = app.request_messages(None);
        assert_eq!(before.len(), 5);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].content, "Be brief");
        assert_eq!(app.system.as_deref(), Some("Be brief"));
        assert_eq!(after[1].role, Role::User);
        assert_eq!(after[1].content, format!("{}\n{SUMMARY}", compact::SUMMARY_HEADER));

        let (context_after, estimated) = app.context_used();
        assert!(context_after < context_before / 4, "{context_before} -> {context_after}");
        assert!(estimated, "the server has not counted the compacted conversation");
        assert_eq!(app.total_usage.input_tokens, 1700, "the summary's tokens were still spent");

        say(&mut app, &mut receiver, "third").await;
        let requests = provider.requests.lock().unwrap();
        let next = &requests[3].messages;
        assert_eq!(next.len(), 2, "strict servers reject two user messages in a row");
        assert!(next[1].content.ends_with(&format!("{SUMMARY}\n\nthird")));
    }

    #[tokio::test]
    async fn the_transcript_marks_the_compaction() {
        let (mut app, mut receiver, _provider) = compactable(None).await;
        say(&mut app, &mut receiver, "/compact").await;
        assert_eq!(app.turns.len(), 2);
        assert!(app.turns[0].summary);
        assert_eq!(app.turns[0].text, SUMMARY);
        assert_eq!(app.turns[1].role, None);
        assert!(app.turns[1].text.starts_with("Compacted 4 messages (~"), "{}", app.turns[1].text);
        assert!(app.turns[1].text.ends_with(" tokens)"), "{}", app.turns[1].text);
    }

    #[tokio::test]
    async fn prefill_and_unsent_files_survive_compaction() {
        let (mut app, mut receiver, provider) = compactable(None).await;
        app.prefill = Some("Sure".into());
        app.fragments = vec!["FILE".into()];
        say(&mut app, &mut receiver, "/compact").await;
        assert_eq!(app.prefill.as_deref(), Some("Sure"));
        assert_eq!(app.fragments, ["FILE"]);
        let requests = provider.requests.lock().unwrap();
        assert!(requests[2].messages.iter().all(|m| m.content != "Sure" && m.content != "FILE"));
    }

    #[tokio::test]
    async fn a_message_that_was_never_answered_is_kept_word_for_word() {
        let store = Store::in_memory().unwrap();
        let (mut app, mut receiver, provider) = compactable(Some(store)).await;
        provider.queued.lock().unwrap().clear();
        type_text(&mut app, "unanswered");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        // Lets this request take its reply before the summary is queued.
        tokio::task::yield_now().await;
        let generation = app.generation;
        app.on_stream(StreamMessage { generation, payload: Payload::Failed("boom".into()) });
        provider.queue(summary_reply());
        say(&mut app, &mut receiver, "/compact").await;

        let requests = provider.requests.lock().unwrap();
        let summarized = &requests.last().unwrap().messages;
        assert!(summarized.iter().all(|message| message.content != "unanswered"));
        drop(requests);

        let unanswered = app.turns.iter().find(|turn| turn.text == "unanswered").unwrap();
        assert_eq!(unanswered.parent_id, app.turns[0].stored_id);
        let last = app.request_messages(None).pop().unwrap();
        assert!(last.content.ends_with(&format!("{SUMMARY}\n\nunanswered")));

        let store = app.store.as_ref().unwrap();
        let thread = store.thread(app.conversation_id.as_ref().unwrap()).unwrap();
        assert_eq!(thread.last().unwrap().content, "unanswered");
        assert_eq!(thread.last().unwrap().id, *unanswered.stored_id.as_ref().unwrap());
    }

    #[tokio::test]
    async fn stopping_a_compaction_changes_nothing_and_ignores_late_events() {
        for key in [KeyCode::Esc, KeyCode::Char('c')] {
            let modifiers =
                if key == KeyCode::Esc { KeyModifiers::NONE } else { KeyModifiers::CONTROL };
            let store = Store::in_memory().unwrap();
            let (mut app, _receiver, _provider) = compactable(Some(store)).await;
            let turns = roles_and_text(&app);
            let context = app.context_used();
            type_text(&mut app, "/compact");
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            assert!(app.busy());

            let generation = app.generation;
            deliver(&mut app, StreamEvent::Text("half a sum".into()));
            press(&mut app, key, modifiers);
            assert!(!app.busy());
            assert!(!app.should_quit);
            assert!(!app.exit_pending());
            assert!(app.notice.as_deref().unwrap().contains("Compaction stopped"));

            app.on_stream(StreamMessage {
                generation,
                payload: Payload::Event(StreamEvent::Text("mary".into())),
            });
            app.on_stream(StreamMessage { generation, payload: Payload::Ended });
            assert_eq!(roles_and_text(&app), turns);
            assert_eq!(app.context_used(), context);
            assert!(!app.turns.last().unwrap().interrupted, "the last reply was not stopped");

            let store = app.store.as_ref().unwrap();
            assert_eq!(store.recent(1).unwrap()[0].messages, 5);
        }
    }

    #[tokio::test]
    async fn a_reply_can_follow_a_stopped_compaction() {
        let (mut app, mut receiver, provider) = compactable(None).await;
        provider.queued.lock().unwrap().clear();
        type_text(&mut app, "/compact");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        say(&mut app, &mut receiver, "third").await;
        assert!(app.turns.last().unwrap().is_completed_reply());
        assert_eq!(app.turns.len(), 6);
    }

    /// Runs `/compact` against a reply and returns the notice left in the transcript.
    async fn failed_compaction(reply: Vec<StreamEvent>) -> String {
        let store = Store::in_memory().unwrap();
        let (mut app, mut receiver, provider) = compactable(Some(store)).await;
        provider.queued.lock().unwrap().clear();
        provider.queue(reply);
        let turns = roles_and_text(&app);
        say(&mut app, &mut receiver, "/compact").await;
        assert_eq!(roles_and_text(&app)[..turns.len()], turns[..]);
        assert_eq!(app.turns.len(), turns.len() + 1);
        assert_eq!(app.store.as_ref().unwrap().recent(1).unwrap()[0].messages, 5);
        app.turns.pop().unwrap().text
    }

    #[tokio::test]
    async fn an_empty_summary_is_rejected() {
        let notice = failed_compaction(vec![StreamEvent::Text(" \n".into())]).await;
        assert!(notice.contains("empty summary"), "{notice}");
    }

    #[tokio::test]
    async fn a_summary_cut_off_by_the_token_limit_is_rejected() {
        let reply =
            vec![StreamEvent::Text("They settled".into()), StreamEvent::Finished("length".into())];
        let notice = failed_compaction(reply).await;
        assert!(notice.contains("cut off"), "{notice}");
    }

    #[tokio::test]
    async fn a_summary_that_saves_nothing_is_rejected() {
        let notice = failed_compaction(vec![StreamEvent::Text("word ".repeat(1000))]).await;
        assert!(notice.contains("no shorter"), "{notice}");
    }

    #[tokio::test]
    async fn a_server_error_leaves_the_conversation_alone() {
        let (mut app, _receiver, provider) = compactable(None).await;
        provider.queued.lock().unwrap().clear();
        let turns = app.turns.len();
        type_text(&mut app, "/compact");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let generation = app.generation;
        app.on_stream(StreamMessage { generation, payload: Payload::Failed("boom".into()) });
        assert!(!app.busy());
        assert_eq!(app.turns.len(), turns + 1);
        assert!(app.turns.last().unwrap().text.ends_with("unchanged: boom"));
        assert!(app.turns[turns - 1].error.is_none(), "the earlier reply did not fail");
    }

    #[tokio::test]
    async fn a_summary_that_cannot_be_saved_is_not_applied() {
        let store = Store::in_memory().unwrap();
        let (mut app, mut receiver, _provider) = compactable(Some(store)).await;
        app.conversation_id = Some("missing".into());
        let turns = roles_and_text(&app);
        say(&mut app, &mut receiver, "/compact").await;
        assert_eq!(roles_and_text(&app)[..turns.len()], turns[..]);
        let notice = &app.turns.last().unwrap().text;
        assert!(notice.contains("could not save the summary"), "{notice}");
    }

    #[tokio::test]
    async fn compaction_works_without_saving_history() {
        let (mut app, mut receiver, _provider) = compactable(None).await;
        say(&mut app, &mut receiver, "/compact").await;
        assert!(app.turns[0].summary);
        assert_eq!(app.turns[0].stored_id, None);
    }

    #[tokio::test]
    async fn a_resumed_chat_continues_from_the_summary_and_keeps_the_originals() {
        let store = Store::in_memory().unwrap();
        let (mut app, mut receiver, provider) = app_with(long_reply(), Some(store));
        app.system = Some("Be brief".into());
        say(&mut app, &mut receiver, "first").await;
        say(&mut app, &mut receiver, "second").await;
        provider.queue(summary_reply());
        say(&mut app, &mut receiver, "/compact").await;

        let conversation = app.conversation_id.clone().unwrap();
        let store = app.store.take().unwrap();
        assert_eq!(store.recent(1).unwrap()[0].messages, 6, "the originals are still stored");
        let history = store.thread(&conversation).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, Role::System);
        assert!(history[1].compacted_from.is_some());

        let (mut resumed, mut receiver) = App::new(session_with(
            provider.clone(),
            Some(store),
            Some(conversation.clone()),
            history,
        ));
        assert_eq!(resumed.system.as_deref(), Some("Be brief"));
        assert_eq!(resumed.turns.len(), 1);
        assert!(resumed.turns[0].summary);
        assert_eq!(resumed.turns[0].text, SUMMARY, "the header is not shown after resuming");

        say(&mut resumed, &mut receiver, "third").await;
        let requests = provider.requests.lock().unwrap();
        let sent = &requests.last().unwrap().messages;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].content, "Be brief");
        let expected = format!("{}\n{SUMMARY}\n\nthird", compact::SUMMARY_HEADER);
        assert_eq!(sent[1].content, expected);

        let thread = resumed.store.as_ref().unwrap().thread(&conversation).unwrap();
        assert_eq!(thread.len(), 4, "the new exchange continues the compacted branch");
        assert_eq!(thread[2].content, "third");
    }

    #[tokio::test]
    async fn a_second_compaction_replaces_the_first_summary() {
        let store = Store::in_memory().unwrap();
        let (mut app, mut receiver, provider) = compactable(Some(store)).await;
        say(&mut app, &mut receiver, "/compact").await;
        say(&mut app, &mut receiver, "third").await;
        provider.queue(vec![StreamEvent::Text("Newer summary".into())]);
        say(&mut app, &mut receiver, "/compact").await;

        assert_eq!(app.turns.iter().filter(|turn| turn.summary).count(), 1);
        assert_eq!(app.turns[0].text, "Newer summary");
        let store = app.store.as_ref().unwrap();
        let thread = store.thread(app.conversation_id.as_ref().unwrap()).unwrap();
        let contents: Vec<_> = thread.iter().map(|message| message.content.as_str()).collect();
        let expected = format!("{}\nNewer summary", compact::SUMMARY_HEADER);
        assert_eq!(contents, ["Be brief", expected.as_str()]);
    }

    #[tokio::test]
    async fn retry_and_edit_have_no_reply_to_act_on_after_compaction() {
        let (mut app, mut receiver, provider) = compactable(None).await;
        say(&mut app, &mut receiver, "/compact").await;
        let sent = provider.requests.lock().unwrap().len();
        say(&mut app, &mut receiver, "/retry").await;
        assert_eq!(app.notice.as_deref(), Some("There is no reply to regenerate"));
        say(&mut app, &mut receiver, "/edit").await;
        assert_eq!(app.notice.as_deref(), Some("There is no reply to edit yet"));
        assert_eq!(provider.requests.lock().unwrap().len(), sent);
    }

    #[tokio::test]
    async fn a_summary_is_never_used_to_name_the_chat() {
        let (mut app, mut receiver, provider) = compactable(None).await;
        say(&mut app, &mut receiver, "/compact").await;
        app.smart_titles = true;
        say(&mut app, &mut receiver, "plan the migration").await;
        let title = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(request) = provider.requests.lock().unwrap().last().cloned()
                    && request.messages.len() == 2
                    && request.messages[0].content.starts_with("Write a short title")
                {
                    return request;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a title is requested");
        assert_eq!(title.messages[1].content, "plan the migration");
    }

    #[tokio::test]
    async fn ctrl_c_during_a_reply_stops_it_without_exiting() {
        let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
        type_text(&mut app, "hi");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.busy());
        assert!(!app.should_quit);
        assert!(!app.exit_pending());
    }
}

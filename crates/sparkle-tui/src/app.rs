//! Chat state and behaviour, independent of the real terminal.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use sparkle_core::personas;
use sparkle_core::prefill::PrefillFilter;
use sparkle_core::title;
use sparkle_core::{ChatRequest, Message, Options, Provider, Role, StreamEvent, Usage};
use sparkle_store::{NewMessage, Store, StoredMessage};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders};
use ratatui_textarea::TextArea;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::theme::{Icons, Theme};

pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show commands and keys"),
    ("/new", "Start a new conversation"),
    ("/model", "Switch model: /model NAME"),
    ("/effort", "Set reasoning: /effort low|medium|high|auto"),
    ("/persona", "Use a built-in persona: /persona NAME|off"),
    ("/system", "Set the system prompt: /system TEXT"),
    ("/prefill", "Start the next reply with: /prefill TEXT"),
    ("/retry", "Regenerate the last reply"),
    ("/edit", "Edit the last reply in $EDITOR"),
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
    pub cache: Option<(u16, bool, Vec<Line<'static>>)>,
}

impl Turn {
    fn new(role: Role, text: impl Into<String>) -> Self {
        Self { role: Some(role), text: text.into(), ..Self::default() }
    }

    fn notice(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    /// The full message content the model receives.
    fn wire_text(&self) -> String {
        if self.context.is_empty() {
            self.text.clone()
        } else {
            format!("{}\n{}", self.context, self.text)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Waiting,
    Thinking,
    Writing,
}

#[derive(Debug)]
pub enum Payload {
    Event(StreamEvent),
    Failed(String),
    Ended,
    Title(String),
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
    pub notice: Option<String>,
    pub dirty: bool,
    pub should_quit: bool,
    pub action: Option<Action>,
    pub completion: Option<Completion>,
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
            let mut turn = Turn::new(message.role, message.content);
            turn.reasoning = message.reasoning.unwrap_or_default();
            turn.model.clone_from(&session.model.id);
            turn.parent_id = parent.take();
            parent = Some(message.id.clone());
            turn.stored_id = Some(message.id);
            turns.push(turn);
        }
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
            notice: None,
            dirty: true,
            should_quit: false,
            action: None,
            completion: None,
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
            generation: 0,
            task: None,
            sender,
        };
        (app, receiver)
    }

    pub fn busy(&self) -> bool {
        self.phase != Phase::Idle
    }

    pub fn on_terminal(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => {
                self.input.insert_str(text);
            }
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
            self.notice = Some("Still replying. Press esc to stop.".into());
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
                    self.notice = Some(format!("Switched to {}", self.model));
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
            "/system" => {
                self.system = (!argument.is_empty()).then(|| argument.to_string());
                self.persona = None;
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
            self.notice = Some("Persona cleared".into());
        } else if let Some(persona) = personas::find(name) {
            self.persona = Some(persona.name.to_string());
            self.system = Some(persona.prompt.to_string());
            self.notice = Some(format!("Persona: {}", persona.name));
        } else {
            self.notice = Some(format!("Unknown persona '{name}'"));
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
        let first = self.turns.iter().find(|turn| turn.role == Some(Role::User));
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
        let mut messages = Vec::new();
        if let Some(system) = &self.system {
            messages.push(Message::new(Role::System, system.clone()));
        }
        for turn in &self.turns {
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
        if let Some(prefill) = prefill {
            messages.push(Message::new(Role::Assistant, prefill));
        }
        messages
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
        self.started = Some(Instant::now());
        self.first_reasoning = None;
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
        if message.generation != self.generation || !self.busy() {
            return;
        }
        self.dirty = true;
        match message.payload {
            Payload::Event(event) => self.on_event(event),
            Payload::Failed(error) => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.error = Some(error);
                }
                self.finish();
            }
            Payload::Ended => self.finish(),
            Payload::Title(_) => {}
        }
    }

    fn elapsed_ms(since: Option<Instant>) -> Option<u64> {
        since.map(|start| start.elapsed().as_millis() as u64)
    }

    fn on_event(&mut self, event: StreamEvent) {
        let thought = Self::elapsed_ms(self.first_reasoning);
        let Some(turn) = self.turns.last_mut() else { return };
        turn.cache = None;
        match event {
            StreamEvent::Reasoning(text) => {
                turn.reasoning.push_str(&text);
                self.first_reasoning.get_or_insert_with(Instant::now);
                self.phase = Phase::Thinking;
            }
            StreamEvent::Text(text) => {
                turn.text.push_str(&self.reply_filter.push(&text));
                turn.thought_ms = turn.thought_ms.or(thought);
                self.phase = Phase::Writing;
            }
            StreamEvent::Usage(usage) => {
                turn.usage = Some(usage);
                self.total_usage.input_tokens += usage.input_tokens;
                self.total_usage.output_tokens += usage.output_tokens;
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
    use std::sync::Mutex;

    pub(crate) struct FakeProvider {
        pub events: Vec<StreamEvent>,
        pub requests: Mutex<Vec<ChatRequest>>,
    }

    impl Provider for FakeProvider {
        fn stream<'a>(&'a self, request: &'a ChatRequest) -> BoxFuture<'a, Result<EventStream>> {
            self.requests.lock().unwrap().push(request.clone());
            let events = self.events.clone();
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
        let provider = Arc::new(FakeProvider { events, requests: Mutex::new(Vec::new()) });
        let session = ChatSession {
            model: ResolvedModel {
                id: "mock".into(),
                wire_name: "mock-wire".into(),
                provider: provider.clone(),
            },
            resolver: Box::new(|name| Err(format!("unknown model {name}"))),
            system: None,
            persona: None,
            options: Options::default(),
            prefill: None,
            fragments: Vec::new(),
            store,
            conversation_id: None,
            history: Vec::new(),
            theme: Theme::DUSK,
            icons: Icons::ASCII,
            animations: true,
            show_reasoning: true,
            title: None,
            smart_titles: false,
        };
        let (app, receiver) = App::new(session);
        (app, receiver, provider)
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

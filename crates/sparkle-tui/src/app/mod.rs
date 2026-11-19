//! Chat state and behaviour, independent of the real terminal.

mod commands;
mod compaction;
mod picker;
mod stream;
#[cfg(test)]
pub(crate) mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use futures_util::StreamExt;
use sparkle_core::compact;
use sparkle_core::context::{self, Fit, estimate_tokens};
use sparkle_core::journal::{self, Journal, Opening};
use sparkle_core::setting;
use sparkle_core::library;
use sparkle_core::prefill::PrefillFilter;
use sparkle_core::title;
use sparkle_core::{Attachment, Message, Options, PromptProgress, Provider, Role, StreamEvent, Usage};
use sparkle_store::{CompactionInfo, NewMessage, Store, StoredMessage};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders};
use ratatui_textarea::{TextArea, WrapMode};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::picker::Picker;
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
    ("/compact", "Compact earlier messages: /compact [summary|handoff|shake|trim] [FOCUS]"),
    ("/journal", "Read the model's journal: /journal [clear|path]"),
    ("/quit", "Leave the chat"),
];

const EFFORTS: [Option<&str>; 4] = [None, Some("low"), Some("medium"), Some("high")];

/// Lines one wheel step scrolls the conversation.
const WHEEL_LINES: usize = 3;

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
    /// Times to resend a request that failed before any reply arrived.
    pub retries: u32,
    /// Where ctrl+t saves whether thinking is shown, for the next run.
    pub show_thinking_file: Option<PathBuf>,
    /// The model's journal, when it is on.
    pub journal: Option<Journal>,
    /// Tell the model she is in sparkle, ahead of any chosen system prompt.
    pub sparkle_context: bool,
    /// Take mouse wheel events, so the wheel scrolls the conversation.
    pub mouse: bool,
    /// Where the default system prompt is saved. Without one, defaults cannot be changed.
    pub default_prompt_file: Option<PathBuf>,
}

#[derive(Default)]
pub struct Turn {
    pub role: Option<Role>,
    pub text: String,
    /// Attached file contents, sent to the model but not drawn in the transcript.
    pub context: String,
    pub attachments: Vec<Attachment>,
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
    /// True for a compaction boundary/summary that stands in for earlier turns on the wire.
    pub summary: bool,
    /// Method and token counts recorded when this turn compacted earlier history.
    pub compaction: Option<CompactionInfo>,
    /// What this reply did to the journal.
    pub journaled: journal::Applied,
    pub cache: Option<(u16, bool, Vec<Line<'static>>)>,
}

impl Turn {
    fn new(role: Role, text: impl Into<String>) -> Self {
        Self { role: Some(role), text: text.into(), ..Self::default() }
    }

    fn notice(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    /// A compaction summary replacing earlier turns on the wire. The header reaches the model
    /// while the transcript renders a divider (and optional expandable summary).
    fn summary(text: impl Into<String>, compaction: Option<CompactionInfo>) -> Self {
        Self {
            context: compact::SUMMARY_HEADER.to_string(),
            summary: true,
            compaction,
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
pub(super) struct Compaction {
    /// Index where the compaction divider will be inserted (right after the last completed reply).
    pub(super) replaced: usize,
    pub(super) method: compact::Method,
    pub(super) reply: String,
    pub(super) truncated: bool,
    pub(super) usage: Option<Usage>,
}

/// A failed request waiting to be sent again.
#[derive(Clone, Debug)]
pub struct Retry {
    pub attempt: u32,
    pub max: u32,
    /// When the next attempt goes out.
    pub at: Instant,
    pub reason: String,
}

#[derive(Debug)]
pub enum Payload {
    Event(StreamEvent),
    Retrying(Retry),
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
    /// Expand compaction divider summaries in the transcript (`ctrl+o`).
    pub show_compaction_summary: bool,
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
    show_thinking_file: Option<PathBuf>,
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
    /// The pending retry of a request that failed before producing anything.
    pub retrying: Option<Retry>,
    /// The first wait between retries; later waits double.
    pub retry_base: Duration,
    pub max_retries: u32,
    /// Messages typed while busy, sent in order once the reply finishes.
    pub queued: Vec<String>,
    /// Everything submitted, oldest first, for recall with up and down.
    input_history: Vec<String>,
    /// The entry being shown while moving through `input_history`.
    history_index: Option<usize>,
    /// What was typed before moving through the history.
    history_draft: String,
    pub journal: Option<Journal>,
    /// The journal as this conversation opened with it. It is not refreshed mid-conversation,
    /// so the start of every request stays the same and server prompt caches stay valid.
    pub journal_opening: Option<Opening>,
    sparkle_context: bool,
    /// The note about sparkle as this conversation opened with it; fixed for the same reason as
    /// the journal, since it names today's date.
    pub setting_note: Option<String>,
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
            let mut turn = if message.is_compaction() {
                Turn::summary(compact::summary_body(&message.content), message.compaction)
            } else {
                Turn::new(message.role, message.content)
            };
            turn.attachments = message.attachments;
            turn.reasoning = message.reasoning.unwrap_or_default();
            turn.model.clone_from(&session.model.id);
            turn.parent_id = parent.take();
            parent = Some(message.id.clone());
            turn.stored_id = Some(message.id);
            turns.push(turn);
        }
        let input_history = turns
            .iter()
            .filter(|turn| turn.role == Some(Role::User) && !turn.summary && !turn.text.is_empty())
            .map(|turn| turn.text.clone())
            .collect();
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
            show_compaction_summary: false,
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
            show_thinking_file: session.show_thinking_file,
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
            retrying: None,
            retry_base: Duration::from_secs(1),
            max_retries: session.retries,
            queued: Vec::new(),
            input_history,
            history_index: None,
            history_draft: String::new(),
            journal_opening: session.journal.as_ref().map(Journal::opening),
            setting_note: session
                .sparkle_context
                .then(|| setting::note(&library::today(), session.journal.is_some())),
            sparkle_context: session.sparkle_context,
            journal: session.journal,
            generation: 0,
            task: None,
            sender,
        };
        (app, receiver)
    }

    pub fn busy(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// Whether the screen changes on its own and needs the frame ticker.
    pub fn animating(&self) -> bool {
        self.busy() || (self.animations && self.turns.is_empty() && self.picker.is_none())
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

    /// Index of the latest compaction boundary in `self.turns`, or `0` if none has run.
    pub(super) fn active_turns_start(&self) -> usize {
        self.turns.iter().rposition(|turn| turn.summary).unwrap_or(0)
    }

    /// Tokens the next request will use, and whether that number is an estimate.
    pub fn context_used(&self) -> (u64, bool) {
        if let Some(progress) = self.prompt_progress {
            return (progress.total, false);
        }
        let (measured, counted) =
            self.measured_context.unwrap_or_else(|| (0, self.active_turns_start()));
        let mut pending: usize = self.turns[counted.min(self.turns.len())..]
            .iter()
            .filter(|turn| turn.role.is_some())
            .map(Turn::wire_len)
            .sum();
        pending += self.fragments.iter().map(String::len).sum::<usize>();
        pending += self.prefill.as_ref().map_or(0, String::len);
        if self.measured_context.is_none() {
            pending += self.system.as_ref().map_or(0, String::len) + self.added_system_len();
        }
        (measured + estimate_tokens(pending), self.measured_context.is_none() || pending > 0)
    }

    /// Estimated tokens for the next request if `prompt_bytes` replaced the system prompt.
    pub fn tokens_with_prompt(&self, prompt_bytes: usize) -> u64 {
        let turns: usize =
            self.turns.iter().filter(|turn| turn.role.is_some()).map(Turn::wire_len).sum();
        let fragments: usize = self.fragments.iter().map(String::len).sum();
        let prefill = self.prefill.as_ref().map_or(0, String::len);
        estimate_tokens(prompt_bytes + self.added_system_len() + turns + fragments + prefill)
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
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.on_wheel(-1),
                MouseEventKind::ScrollDown => self.on_wheel(1),
                // Clicks and drags mean nothing here; skip the redraw.
                _ => return,
            },
            Event::Resize(..) => {}
            _ => return,
        }
        self.dirty = true;
    }

    /// The wheel scrolls whatever is being read: the prompt browser's list when it is open,
    /// otherwise the conversation. It never touches the input or its history.
    fn on_wheel(&mut self, direction: isize) {
        if let Some(picker) = &mut self.picker {
            picker.move_by(direction);
            return;
        }
        self.scroll = match direction {
            up if up < 0 => self.scroll.saturating_add(WHEEL_LINES),
            _ => self.scroll.saturating_sub(WHEEL_LINES),
        };
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
                if let Some(file) = &self.show_thinking_file
                    && let Err(error) = sparkle_core::config::save_switch(file, self.show_reasoning)
                {
                    self.notice = Some(format!("Could not remember the thinking setting: {error}"));
                }
            }
            KeyCode::Char('o') if control => {
                self.show_compaction_summary = !self.show_compaction_summary;
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
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Up if self.input_is_empty() && !self.queued.is_empty() => {
                // Takes back the latest queued message to change it before it goes.
                let text = self.queued.pop().unwrap_or_default();
                self.set_input(&text);
            }
            KeyCode::Up if self.input.cursor().0 == 0 => self.recall_older(),
            KeyCode::Down
                if self.history_index.is_some()
                    && self.input.cursor().0 + 1 >= self.input.lines().len() =>
            {
                self.recall_newer();
            }
            _ => {
                if self.input.input(key) {
                    // Editing a recalled message makes it the new draft.
                    self.history_index = None;
                }
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

    fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    /// Remembers a submitted message for recall, skipping an immediate repeat.
    fn remember_input(&mut self, text: &str) {
        self.history_index = None;
        self.history_draft.clear();
        if self.input_history.last().is_none_or(|last| last != text) {
            self.input_history.push(text.to_string());
        }
    }

    /// Shows the previous submitted message, keeping the draft for the way back down.
    fn recall_older(&mut self) {
        let index = match self.history_index {
            Some(0) => return,
            Some(index) => index - 1,
            None if self.input_history.is_empty() => return,
            None => {
                self.history_draft = self.input_text();
                self.input_history.len() - 1
            }
        };
        self.history_index = Some(index);
        let text = self.input_history[index].clone();
        self.set_input(&text);
        // From the top of a long entry, the next up goes straight to the one before it.
        self.input.move_cursor(ratatui_textarea::CursorMove::Top);
        self.input.move_cursor(ratatui_textarea::CursorMove::Head);
    }

    /// Shows the next submitted message, then the draft once past the newest.
    fn recall_newer(&mut self) {
        let Some(index) = self.history_index else { return };
        if index + 1 < self.input_history.len() {
            self.history_index = Some(index + 1);
            let text = self.input_history[index + 1].clone();
            self.set_input(&text);
        } else {
            self.history_index = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.set_input(&draft);
        }
    }

    /// Sends queued messages in order until one starts a reply.
    fn run_queued(&mut self) {
        while !self.busy() && !self.queued.is_empty() {
            let text = self.queued.remove(0);
            self.dispatch(text);
        }
    }

    /// Puts queued messages back in the input after a reply stops early, so nothing is sent
    /// on top of a failure the user has not seen.
    fn restore_queued(&mut self) {
        if self.queued.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.queued);
        let current = self.input_text();
        if !current.trim().is_empty() {
            parts.push(current);
        }
        self.set_input(&parts.join("\n\n"));
        self.notice = Some("Queued messages are back in the input".into());
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


    fn last_reply(&self) -> Option<usize> {
        let start = self.active_turns_start();
        self.turns[start..]
            .iter()
            .rposition(|turn| turn.role == Some(Role::Assistant))
            .map(|offset| start + offset)
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

    /// Bytes sparkle adds around the system prompt the person chose.
    fn added_system_len(&self) -> usize {
        self.wire_system().map_or(0, |system| system.len())
            - self.system.as_ref().map_or(0, String::len)
    }

    /// The system prompt as the model receives it: where she is, then the chosen prompt, then
    /// her journal, and a closing reminder when a chosen prompt could pull her elsewhere.
    pub fn wire_system(&self) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        parts.extend(self.setting_note.as_deref());
        parts.extend(self.system.as_deref());
        parts.extend(self.journal_opening.as_ref().map(|opening| opening.text.as_str()));
        if self.setting_note.is_some() && self.system.is_some() {
            parts.push(setting::REMINDER);
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Opens the journal and the note about sparkle afresh for a new conversation.
    pub(super) fn reopen_journal(&mut self) {
        self.journal_opening = self.journal.as_ref().map(Journal::opening);
        self.setting_note = self
            .sparkle_context
            .then(|| setting::note(&library::today(), self.journal.is_some()));
    }

    /// Keeps the notes the model wrote in a finished reply, and counts them on the reply.
    pub(super) fn keep_journal_notes(&mut self) {
        let (Some(journal), Some(turn)) = (&self.journal, self.turns.last_mut()) else { return };
        let actions = journal::actions(&turn.text);
        if actions.is_empty() {
            return;
        }
        match journal.apply(&actions, Some(&turn.model)) {
            Ok(applied) => {
                turn.journaled = applied;
                turn.cache = None;
            }
            Err(error) => self.notice = Some(format!("Could not save to the journal: {error}")),
        }
    }

    /// The system prompt followed by the active slice of `turns` (starting at the latest
    /// compaction boundary, if any), as the model receives them.
    fn wire_messages(&self, turns: &[Turn]) -> Vec<Message> {
        let mut messages = Vec::new();
        if let Some(system) = self.wire_system() {
            messages.push(Message::new(Role::System, system));
        }
        let start = turns.iter().rposition(|turn| turn.summary).unwrap_or(0);
        for turn in &turns[start..] {
            let Some(role) = turn.role.filter(|_| !turn.text.is_empty() || !turn.attachments.is_empty())
            else {
                continue;
            };
            match messages.last_mut() {
                // After a failed reply or compaction summary two user messages meet, and strict
                // servers reject that.
                Some(previous) if role == Role::User && previous.role == Role::User => {
                    previous.content.push_str("\n\n");
                    previous.content.push_str(&turn.wire_text());
                    previous.attachments.extend(turn.attachments.iter().cloned());
                }
                _ => {
                    let mut message = Message::new(role, turn.wire_text());
                    message.attachments = turn.attachments.clone();
                    messages.push(message);
                }
            }
        }
        messages
    }

}

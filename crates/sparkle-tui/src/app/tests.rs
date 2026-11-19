use super::*;
use futures_util::future::BoxFuture;
use futures_util::stream;
use sparkle_core::{ChatRequest, EventStream, Result};
use std::collections::VecDeque;
use std::sync::Mutex;

pub(crate) struct FakeProvider {
    pub events: Vec<StreamEvent>,
    /// Replies for the next requests, used ahead of `events`.
    pub queued: Mutex<VecDeque<Vec<StreamEvent>>>,
    pub requests: Mutex<Vec<ChatRequest>>,
    /// Errors returned instead of a stream, one per request, before any reply is used.
    pub failures: Mutex<VecDeque<sparkle_core::Error>>,
}

impl FakeProvider {
    pub(crate) fn queue(&self, events: Vec<StreamEvent>) {
        self.queued.lock().unwrap().push_back(events);
    }
}

impl Provider for FakeProvider {
    fn stream<'a>(&'a self, request: &'a ChatRequest) -> BoxFuture<'a, Result<EventStream>> {
        self.requests.lock().unwrap().push(request.clone());
        if let Some(error) = self.failures.lock().unwrap().pop_front() {
            return Box::pin(async move { Err(error) });
        }
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
        failures: Mutex::new(VecDeque::new()),
    });
    let (mut app, receiver) = App::new(session_with(provider.clone(), store, None, Vec::new()));
    app.retry_base = Duration::ZERO;
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
        retries: 5,
        show_thinking_file: None,
        journal: None,
        sparkle_context: false,
        mouse: false,
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
    assert!(COMMANDS.iter().any(|(name, _)| *name == "/compact"));
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
async fn compaction_waits_in_the_queue_while_a_reply_is_streaming() {
    let (mut app, _receiver, provider) = app_with(long_reply(), None);
    type_text(&mut app, "hi");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/compact");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.queued, vec!["/compact".to_string()], "the command is not lost");
    assert!(app.input_is_empty());
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
    assert_eq!(app.queued, vec!["next question".to_string()]);
    assert_eq!(app.phase, Phase::Compacting);
}

#[tokio::test]
async fn queued_messages_go_out_in_order_after_the_reply() {
    let (mut app, mut receiver, provider) = app_with(reply_events(), None);
    type_text(&mut app, "one");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "two");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "three");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.queued.len(), 2);
    drain(&mut app, &mut receiver).await;
    assert!(app.queued.is_empty());
    let users: Vec<String> = app
        .turns
        .iter()
        .filter(|turn| turn.role == Some(Role::User))
        .map(|turn| turn.text.clone())
        .collect();
    assert_eq!(users, ["one", "two", "three"]);
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn stopping_a_reply_puts_queued_messages_back_in_the_input() {
    let (mut app, _receiver, provider) = app_with(long_reply(), None);
    type_text(&mut app, "one");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "two");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.busy());
    assert!(app.queued.is_empty());
    assert_eq!(app.input.lines().join("\n"), "two");
    tokio::task::yield_now().await;
    assert!(provider.requests.lock().unwrap().len() <= 1, "nothing was sent after stopping");
    let users = app.turns.iter().filter(|turn| turn.role == Some(Role::User)).count();
    assert_eq!(users, 1);
}

#[tokio::test]
async fn up_takes_back_the_latest_queued_message() {
    let (mut app, _receiver, _provider) = app_with(long_reply(), None);
    type_text(&mut app, "one");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "two");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert!(app.queued.is_empty());
    assert_eq!(app.input.lines().join(""), "two");
}

#[tokio::test]
async fn up_and_down_move_through_sent_messages_and_keep_the_draft() {
    let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
    say(&mut app, &mut receiver, "first").await;
    say(&mut app, &mut receiver, "second\nline").await;
    type_text(&mut app, "draft");
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input.lines().join("\n"), "second\nline");
    // The cursor lands at the top, so the next up goes on to the older message.
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input.lines().join("\n"), "first");
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input.lines().join("\n"), "first", "stops at the oldest");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.input.lines().join("\n"), "second\nline");
    // Coming back down leaves the cursor at the end, so down goes on to the draft.
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.input.lines().join("\n"), "draft");
    // Resending a recalled message is just enter.
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    drain(&mut app, &mut receiver).await;
    let last_user = app.turns.iter().rev().find(|turn| turn.role == Some(Role::User)).unwrap();
    assert_eq!(last_user.text, "first");
}

#[tokio::test]
async fn shift_up_scrolls_the_transcript() {
    let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
    press(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert_eq!(app.scroll, 1);
    press(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
    assert_eq!(app.scroll, 0);
}

#[tokio::test]
async fn transient_failures_are_retried_before_anything_arrives() {
    let (mut app, mut receiver, provider) = app_with(reply_events(), None);
    let busy = || sparkle_core::Error::Api { status: 503, message: "busy".into() };
    provider.failures.lock().unwrap().extend([busy(), busy()]);
    type_text(&mut app, "hi");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let mut saw_retry = false;
    while app.busy() {
        let message = receiver.recv().await.unwrap();
        app.on_stream(message);
        saw_retry |= app.retrying.is_some();
    }
    assert!(saw_retry);
    assert!(app.retrying.is_none());
    let reply = app.turns.last().unwrap();
    assert_eq!((reply.text.as_str(), reply.error.as_ref()), ("Hello **there**", None));
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn permanent_failures_and_exhausted_retries_are_reported() {
    let (mut app, mut receiver, provider) = app_with(reply_events(), None);
    let denied = sparkle_core::Error::Api { status: 401, message: "no key".into() };
    provider.failures.lock().unwrap().push_back(denied);
    say(&mut app, &mut receiver, "hi").await;
    assert!(app.turns.last().unwrap().error.as_ref().unwrap().contains("401"));
    assert_eq!(provider.requests.lock().unwrap().len(), 1, "a 401 is not retried");

    let busy = || sparkle_core::Error::Api { status: 429, message: "slow down".into() };
    provider.failures.lock().unwrap().extend((0..10).map(|_| busy()));
    say(&mut app, &mut receiver, "again").await;
    assert!(app.turns.last().unwrap().error.as_ref().unwrap().contains("429"));
    assert_eq!(provider.requests.lock().unwrap().len(), 2 + app.max_retries as usize);
}

#[test]
fn backoff_doubles_and_is_capped() {
    let base = Duration::from_secs(1);
    let first = super::stream::backoff(base, 1);
    let third = super::stream::backoff(base, 3);
    assert!(first >= base && first <= base.mul_f64(1.25), "{first:?}");
    assert!(third >= base * 4 && third <= base.mul_f64(5.0), "{third:?}");
    assert!(super::stream::backoff(base, 20) <= Duration::from_secs(38));
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
    // Prior 4 turns stay visible above the inline compaction divider.
    assert_eq!(app.turns.len(), 5);
    assert_eq!(app.turns[0].text, "first");
    assert!(app.turns[4].summary);
    assert_eq!(app.turns[4].text, SUMMARY);
    let info = app.turns[4].compaction.as_ref().unwrap();
    assert_eq!(info.method, "summary");
    assert!(info.tokens_before > info.tokens_after);
    let notice = app.notice.as_deref().unwrap();
    assert!(notice.starts_with("Compacted 4 messages via summary (~"), "{notice}");
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

    let summary = app.turns.iter().find(|turn| turn.summary).unwrap();
    let unanswered = app.turns.iter().find(|turn| turn.text == "unanswered").unwrap();
    assert_eq!(unanswered.parent_id, summary.stored_id);
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
    let summary = app.turns.last().unwrap();
    assert!(summary.summary);
    assert_eq!(summary.stored_id, None);
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
    // Display transcript preserves every entry chronologically: system + 4 turns + compaction.
    assert_eq!(history.len(), 6);
    assert_eq!(history[0].role, Role::System);
    assert!(history[5].is_compaction());

    let (mut resumed, mut receiver) = App::new(session_with(
        provider.clone(),
        Some(store),
        Some(conversation.clone()),
        history,
    ));
    assert_eq!(resumed.system.as_deref(), Some("Be brief"));
    assert_eq!(resumed.turns.len(), 5);
    assert_eq!(resumed.turns[0].text, "first");
    assert!(resumed.turns[4].summary);
    assert_eq!(resumed.turns[4].text, SUMMARY, "the header is not shown after resuming");

    say(&mut resumed, &mut receiver, "third").await;
    let requests = provider.requests.lock().unwrap();
    let sent = &requests.last().unwrap().messages;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].content, "Be brief");
    let expected = format!("{}\n{SUMMARY}\n\nthird", compact::SUMMARY_HEADER);
    assert_eq!(sent[1].content, expected);

    let thread = resumed.store.as_ref().unwrap().thread(&conversation).unwrap();
    assert_eq!(thread.len(), 8, "the new exchange continues the compacted branch");
    assert_eq!(thread[6].content, "third");
}

#[tokio::test]
async fn a_second_compaction_inserts_a_new_boundary_and_resets_wire_context() {
    let store = Store::in_memory().unwrap();
    let (mut app, mut receiver, provider) = compactable(Some(store)).await;
    say(&mut app, &mut receiver, "/compact").await;
    say(&mut app, &mut receiver, "third").await;
    provider.queue(vec![StreamEvent::Text("Newer summary".into())]);
    say(&mut app, &mut receiver, "/compact").await;

    assert_eq!(app.turns.iter().filter(|turn| turn.summary).count(), 2);
    assert_eq!(app.turns.last().unwrap().text, "Newer summary");
    let wire = app.request_messages(None);
    assert_eq!(wire.len(), 2);
    assert_eq!(wire[1].content, format!("{}\nNewer summary", compact::SUMMARY_HEADER));

    let store = app.store.as_ref().unwrap();
    let thread = store.thread(app.conversation_id.as_ref().unwrap()).unwrap();
    let active = sparkle_store::active_context(&thread);
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].content, format!("{}\nNewer summary", compact::SUMMARY_HEADER));
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
    let (mut app, mut receiver, provider) = app_with(long_reply(), None);
    // Simulate a legacy resumed chat starting directly with a compaction summary.
    app.turns.push(Turn::summary(SUMMARY, None));
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
async fn handoff_shake_and_trim_compaction_methods_work() {
    // Handoff uses the structured handoff prompt and records method = "handoff".
    let (mut app, mut receiver, provider) = compactable(None).await;
    say(&mut app, &mut receiver, "/compact handoff preserve ports").await;
    let requests = provider.requests.lock().unwrap();
    let prompt = &requests[2].messages.last().unwrap().content;
    assert!(prompt.contains("## Goal"), "{prompt}");
    assert!(prompt.ends_with("focus on: preserve ports"), "{prompt}");
    assert_eq!(app.turns.last().unwrap().compaction.as_ref().unwrap().method, "handoff");

    // Trim runs locally with zero LLM calls and records method = "trim".
    let (mut app, mut receiver, provider) = app_with(long_reply(), None);
    say(&mut app, &mut receiver, "first").await;
    say(&mut app, &mut receiver, "second").await;
    let before_requests = provider.requests.lock().unwrap().len();
    say(&mut app, &mut receiver, "/compact trim 2").await;
    assert_eq!(provider.requests.lock().unwrap().len(), before_requests, "trim is local");
    let summary = app.turns.last().unwrap();
    assert!(summary.summary);
    assert_eq!(summary.compaction.as_ref().unwrap().method, "trim");
    assert!(summary.text.contains("Dropped 2 earlier messages"));

    // Shake runs locally, eliding large fenced blocks in older turns.
    let big = format!("Check this:\n```rust\n{}\n```", "fn main() {}\n".repeat(50));
    let (mut app, mut receiver, provider) = app_with(vec![StreamEvent::Text(big)], None);
    say(&mut app, &mut receiver, "show code").await;
    provider.queue(vec![StreamEvent::Text("short answer".into())]);
    say(&mut app, &mut receiver, "follow up").await;
    let before_requests = provider.requests.lock().unwrap().len();
    say(&mut app, &mut receiver, "/compact shake").await;
    assert_eq!(provider.requests.lock().unwrap().len(), before_requests, "shake is local");
    let summary = app.turns.last().unwrap();
    assert!(summary.summary);
    assert_eq!(summary.compaction.as_ref().unwrap().method, "shake");
    assert!(summary.text.contains("[Elided ~"), "{}", summary.text);
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

fn with_journal(app: &mut App, name: &str) -> Journal {
    let path = std::env::temp_dir()
        .join(format!("sparkle-app-journal-{}-{name}", std::process::id()))
        .join("journal.jsonl");
    let journal = Journal::new(path);
    journal.clear().unwrap();
    app.journal = Some(journal.clone());
    app.reopen_journal();
    journal
}

fn noting(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::Text(text.into()), StreamEvent::Finished("stop".into())]
}

#[tokio::test]
async fn the_model_keeps_notes_without_them_showing_in_the_chat() {
    let reply = "Nice to meet you, Sam!\n<journal keep=\"always\">Their name is Sam</journal>";
    let (mut app, mut receiver, provider) = app_with(noting(reply), None);
    app.system = Some("Be kind".into());
    let journal = with_journal(&mut app, "keeps");
    say(&mut app, &mut receiver, "hi, I'm Sam").await;

    let system = &provider.requests.lock().unwrap()[0].messages[0];
    assert!(system.content.starts_with("Be kind\n\nYou keep a journal"), "{}", system.content);
    let entries = journal.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!((entries[0].text.as_str(), entries[0].until), ("Their name is Sam", None));
    assert_eq!(entries[0].model.as_deref(), Some("mock"));
    assert_eq!(app.turns.last().unwrap().journaled, journal::Applied { kept: 1, let_go: 0 });
    // The model still sees what she wrote, so she knows the note is kept.
    assert!(app.turns.last().unwrap().text.contains("<journal"));
    journal.clear().unwrap();
}

#[tokio::test]
async fn the_journal_stays_put_during_a_conversation_and_refreshes_on_new() {
    let (mut app, mut receiver, provider) =
        app_with(noting("Noted. <journal keep=\"2w\">Likes tea</journal>"), None);
    let journal = with_journal(&mut app, "stays");
    say(&mut app, &mut receiver, "I like tea").await;
    say(&mut app, &mut receiver, "and biscuits").await;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[0].messages[0], requests[1].messages[0], "the opening never changes");
        assert!(!requests[1].messages[0].content.contains("Likes tea"));
    }
    say(&mut app, &mut receiver, "/new").await;
    say(&mut app, &mut receiver, "hello again").await;
    let requests = provider.requests.lock().unwrap();
    let opening = &requests[2].messages[0].content;
    assert!(opening.contains("\n- [") && opening.contains("2w left] Likes tea"), "{opening}");
    assert_eq!(journal.entries().len(), 1, "writing the same note again renews it");
    journal.clear().unwrap();
}

#[tokio::test]
async fn the_model_can_let_a_note_go_by_quoting_it() {
    let (mut app, mut receiver, provider) = app_with(Vec::new(), None);
    let journal = with_journal(&mut app, "forget");
    let keep = |text: &str| journal::Action::Keep { text: text.into(), keep: None, about: None };
    journal.apply(&[keep("Their name is Kelsey"), keep("Back soon to test the journal")], None).unwrap();
    app.reopen_journal();
    // A guessed number does nothing; quoting the note lets it go and marks the reply.
    provider.queue(noting("Sure. <journal forget=\"1\"/>"));
    say(&mut app, &mut receiver, "remove the test note").await;
    assert_eq!(journal.entries().len(), 2);
    assert!(!app.turns.last().unwrap().journaled.any());
    provider.queue(noting("Done. <journal forget=\"test the journal\"/>"));
    say(&mut app, &mut receiver, "remove the test note").await;
    let texts: Vec<String> = journal.entries().into_iter().map(|entry| entry.text).collect();
    assert_eq!(texts, ["Their name is Kelsey"]);
    assert_eq!(app.turns.last().unwrap().journaled, journal::Applied { kept: 0, let_go: 1 });

    // A note written earlier in the same conversation can be let go the same way.
    provider.queue(noting("Okay. <journal keep=\"1w\">Kelsey is at a café</journal>"));
    say(&mut app, &mut receiver, "I'm at a café").await;
    provider.queue(noting("Removed. <journal forget=\"at a café\"/>"));
    say(&mut app, &mut receiver, "never mind, remove that").await;
    assert_eq!(journal.entries().len(), 1);
    journal.clear().unwrap();
}

#[tokio::test]
async fn the_person_can_read_and_clear_the_journal() {
    let (mut app, mut receiver, _provider) = app_with(Vec::new(), None);
    say(&mut app, &mut receiver, "/journal").await;
    assert!(app.notice.as_deref().unwrap().contains("journal is off"));

    let journal = with_journal(&mut app, "read");
    say(&mut app, &mut receiver, "/journal").await;
    assert_eq!(app.turns.last().unwrap().text, "The journal is empty");
    let keep = journal::Action::Keep { text: "Likes tea".into(), keep: Some(86_400 * 7), about: None };
    journal.apply(&[keep], None).unwrap();
    say(&mut app, &mut receiver, "/journal").await;
    let listing = &app.turns.last().unwrap().text;
    assert!(listing.contains("1 notes") || listing.contains("Journal, 1"), "{listing}");
    assert!(listing.contains("7d left · Likes tea"), "{listing}");
    say(&mut app, &mut receiver, "/journal clear").await;
    assert!(journal.entries().is_empty());
}

#[tokio::test]
async fn without_a_journal_the_system_prompt_is_unchanged() {
    let (mut app, mut receiver, provider) = app_with(reply_events(), None);
    app.system = Some("Be kind".into());
    say(&mut app, &mut receiver, "hi").await;
    assert_eq!(provider.requests.lock().unwrap()[0].messages[0].content, "Be kind");
}

#[tokio::test]
async fn choosing_a_prompt_remembers_it_for_new_chats_and_turning_it_off_forgets() {
    let (mut app, mut receiver, _provider) = app_with(Vec::new(), None);
    let file = default_file("remember");
    app.default_prompt_file = Some(file.clone());

    // From the browser.
    app.prompts_command("");
    app.picker.as_mut().unwrap().push_filter("gpt-3.5");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(library::saved_default(&file).as_deref(), Some("openai/gpt-3.5"));
    assert!(app.notice.as_deref().unwrap().starts_with("System prompt: openai/gpt-3.5"));

    // By name, and as a persona.
    say(&mut app, &mut receiver, "/prompts concise").await;
    assert_eq!(library::saved_default(&file).as_deref(), Some("concise"));
    say(&mut app, &mut receiver, "/persona socratic").await;
    assert_eq!(library::saved_default(&file).as_deref(), Some("socratic"));

    // Typed text is kept in a file beside the default, and that file is remembered.
    say(&mut app, &mut receiver, "/system Answer in haiku").await;
    let saved = library::saved_default(&file).unwrap();
    assert!(saved.ends_with("custom_prompt.md"), "{saved}");
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "Answer in haiku");
    assert_eq!(library::load(&saved, None).unwrap().text, "Answer in haiku");

    // Each way of turning the prompt off forgets it.
    for off in ["/prompts off", "/persona off", "/system"] {
        say(&mut app, &mut receiver, "/persona socratic").await;
        say(&mut app, &mut receiver, off).await;
        assert_eq!(library::saved_default(&file), None, "{off}");
    }
}

#[tokio::test]
async fn a_prompt_file_chosen_in_the_chat_is_remembered_by_its_full_path() {
    let (mut app, mut receiver, _provider) = app_with(Vec::new(), None);
    let file = default_file("remember-file");
    app.default_prompt_file = Some(file.clone());
    let prompt = file.parent().unwrap().join("mine.md");
    std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
    std::fs::write(&prompt, "Mine").unwrap();
    say(&mut app, &mut receiver, &format!("/prompts file {}", prompt.display())).await;
    let saved = library::saved_default(&file).unwrap();
    assert_eq!(std::path::Path::new(&saved), prompt.canonicalize().unwrap());
}

#[tokio::test]
async fn the_model_is_told_where_she_is_around_the_chosen_prompt() {
    let (mut app, mut receiver, provider) = app_with(reply_events(), None);
    app.sparkle_context = true;
    let journal = with_journal(&mut app, "setting");
    app.system = Some("You are Claude, made by Anthropic.".into());
    say(&mut app, &mut receiver, "hi").await;

    let system = provider.requests.lock().unwrap()[0].messages[0].content.clone();
    let note = system.find("# Where you are").unwrap();
    let chosen = system.find("You are Claude").unwrap();
    let guide = system.find("You keep a journal").unwrap();
    let reminder = system.find(setting::REMINDER).unwrap();
    assert!(note < chosen && chosen < guide && guide < reminder, "{system}");
    assert!(system.contains(&format!("Today's date is {}.", library::today())));
    assert!(system.ends_with(setting::REMINDER));

    // Without a chosen prompt there is nothing to remind her away from.
    say(&mut app, &mut receiver, "/prompts off").await;
    say(&mut app, &mut receiver, "/new").await;
    say(&mut app, &mut receiver, "hi again").await;
    let system = provider.requests.lock().unwrap()[1].messages[0].content.clone();
    assert!(system.starts_with("# Where you are") && !system.contains("Reminder:"), "{system}");
    journal.clear().unwrap();
}

#[tokio::test]
async fn the_note_about_sparkle_can_be_turned_off() {
    let (mut app, mut receiver, provider) = app_with(reply_events(), None);
    app.system = Some("Be kind".into());
    say(&mut app, &mut receiver, "hi").await;
    assert_eq!(provider.requests.lock().unwrap()[0].messages[0].content, "Be kind");
    assert_eq!(app.wire_system().as_deref(), Some("Be kind"));
}

fn wheel(app: &mut App, kind: crossterm::event::MouseEventKind) {
    app.on_terminal(Event::Mouse(crossterm::event::MouseEvent {
        kind,
        column: 10,
        row: 5,
        modifiers: KeyModifiers::NONE,
    }));
}

#[tokio::test]
async fn the_wheel_scrolls_the_conversation_and_leaves_the_input_alone() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let (mut app, mut receiver, _provider) = app_with(reply_events(), None);
    say(&mut app, &mut receiver, "first").await;
    type_text(&mut app, "draft");

    wheel(&mut app, MouseEventKind::ScrollUp);
    wheel(&mut app, MouseEventKind::ScrollUp);
    assert_eq!(app.scroll, 6);
    assert_eq!(app.input.lines().join(""), "draft", "the wheel never recalls history");
    wheel(&mut app, MouseEventKind::ScrollDown);
    assert_eq!(app.scroll, 3);

    // The arrows still recall sent messages.
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input.lines().join(""), "first");

    // Other buttons do nothing and do not even ask for a redraw.
    app.dirty = false;
    wheel(&mut app, MouseEventKind::Down(MouseButton::Right));
    assert!(!app.dirty);
}

#[tokio::test]
async fn the_wheel_moves_through_the_prompt_browser_when_it_is_open() {
    use crossterm::event::MouseEventKind;
    let (mut app, _receiver, _provider) = app_with(Vec::new(), None);
    app.prompts_command("");
    let first = app.picker.as_ref().unwrap().selected().unwrap().name.clone();
    wheel(&mut app, MouseEventKind::ScrollDown);
    let second = app.picker.as_ref().unwrap().selected().unwrap().name.clone();
    assert_ne!(first, second);
    assert_eq!(app.scroll, 0, "the conversation behind the browser stays put");
}

#[test]
fn the_context_reading_starts_at_half_full_in_steps_of_a_tenth() {
    assert_eq!(meter_line(499, 1000), None);
    assert_eq!(
        meter_line(500, 1000).as_deref(),
        Some("[sparkle: about 50% of your 1k-token context window is in use]")
    );
    assert!(meter_line(739, 1000).unwrap().contains("about 70%"));
    assert!(meter_line(5000, 1000).unwrap().contains("about 100%"));
}

#[tokio::test]
async fn the_model_is_told_how_full_her_context_is_without_it_reaching_history() {
    let store = Store::in_memory().unwrap();
    let (mut app, mut receiver, provider) = app_with(reply_events(), Some(store));
    app.sparkle_context = true;
    app.context_window = Some(1000);
    // Small enough to stay under half the window at first.
    say(&mut app, &mut receiver, "hello").await;
    let first = provider.requests.lock().unwrap()[0].messages.clone();
    assert_eq!(first.last().unwrap().content, "hello", "nothing below half full");

    let long = "word ".repeat(500);
    say(&mut app, &mut receiver, &long).await;
    let requests = provider.requests.lock().unwrap();
    let sent = &requests[1].messages;
    let last = &sent.last().unwrap().content;
    assert!(last.starts_with(long.trim_end()) && last.contains("\n\n[sparkle: about "), "{last}");
    // The earlier message is sent exactly as before, so the cached prefix still matches.
    assert_eq!(sent[..first.len()], first[..]);
    // The person's transcript and saved history never see the reading.
    assert_eq!(app.turns[2].text, long.trim_end());
    let thread = app.store.as_ref().unwrap().thread(app.conversation_id.as_ref().unwrap()).unwrap();
    assert!(thread.iter().all(|message| !message.content.contains("[sparkle:")));
}

#[tokio::test]
async fn there_is_no_context_reading_without_the_note_that_explains_it() {
    let (mut app, mut receiver, provider) = app_with(reply_events(), None);
    app.context_window = Some(1000);
    say(&mut app, &mut receiver, &"word ".repeat(500)).await;
    let requests = provider.requests.lock().unwrap();
    assert!(!requests[0].messages.last().unwrap().content.contains("[sparkle:"));
}

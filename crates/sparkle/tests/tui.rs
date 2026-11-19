//! Tests of the real full-screen interface, driven through a pseudo-terminal.

mod common;

use std::time::Duration;

use common::*;
use serde_json::Value;

fn contents(body: &Value) -> Vec<String> {
    let messages = body["messages"].as_array().unwrap();
    messages.iter().map(|message| message["content"].as_str().unwrap().to_string()).collect()
}

fn no_smart_titles(harness: &Harness) {
    // Mock failures are permanent, so waiting to retry them only slows the tests down.
    harness.write_file("config.toml", "smart_titles = false\nretries = 0\n");
}

#[test]
fn opens_with_the_default_model_and_sends_a_message() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.wait_for("Chatting with mock");
    tui.ask(&harness, "hello");
    tui.wait_for("Hello world");
    tui.wait_for("print('hi')");
    assert_eq!(harness.last_body()["model"], "mock-wire");
    tui.quit();
    assert!(tui.raw().contains("Continue this conversation with: sparkle chat --cid"));
}

// Prefill progress and context

#[test]
fn prefill_progress_replaces_connecting_until_the_reply_starts() {
    let harness = Harness::new();
    no_smart_titles(&harness);
    let mut tui = Tui::open(&harness, &[]);
    tui.send("slowly prefill-progress\r");
    tui.wait_for("Prefilling 50% (2k/4k), 1k cached");
    tui.wait_for("Hello world");
    tui.wait_gone("Prefilling");
    assert_eq!(harness.last_body()["return_progress"], true);
    tui.wait_gone("esc to stop");
    tui.quit();
}

#[test]
fn a_local_server_reports_its_context_size() {
    let harness = Harness::new();
    no_smart_titles(&harness);
    let mut tui = Tui::open(&harness, &["-s", "Be brief"]);
    tui.wait_for("Context window: 8k tokens");
    tui.wait_for("sys ~2 tok (8 B)");
    tui.wait_for("~2/8k (0%)");
    tui.ask(&harness, "hello");
    tui.wait_for("18/8k (0%)");
    tui.quit();
}

// Window title

#[test]
fn the_window_title_shows_the_icon_then_the_prompt_then_a_generated_title() {
    let harness = Harness::new();
    let mut tui = Tui::spawn(&harness, &[], &[("TERM", "xterm-ghostty")]);
    tui.wait_for(READY);
    tui.wait_for_window_title("\u{f0674} sparkle");
    tui.send("Why is my parser so slow today?\r");
    harness.wait_for_requests(1);
    tui.wait_for_window_title(&format!("\u{f0674} {GENERATED_TITLE}"));
    assert!(
        tui.window_titles().contains(&"\u{f0674} Why is my parser so slow today?".to_string()),
        "{:?}",
        tui.window_titles()
    );
    assert!(!tui.screen().contains(GENERATED_TITLE), "the title belongs on the tab only");
    tui.quit();
    assert!(tui.raw().contains("\x1b[23;2t"), "the previous title should be restored");
}

#[test]
fn smart_titles_can_be_turned_off() {
    let harness = Harness::new();
    no_smart_titles(&harness);
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "Plan my garden");
    tui.wait_for_window_title("✦ Plan my garden");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(harness.title_requests(), 0);
    tui.quit();
}

#[test]
fn a_resumed_chat_keeps_its_generated_title() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "Plan my garden");
    tui.wait_for_window_title(&format!("✦ {GENERATED_TITLE}"));
    tui.quit();

    let mut resumed = Tui::open(&harness, &["chat", "-c"]);
    resumed.wait_for_window_title(&format!("✦ {GENERATED_TITLE}"));
    resumed.ask(&harness, "more");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(harness.title_requests(), 1);
    resumed.quit();
}

#[test]
fn a_new_chat_resets_the_title() {
    let harness = Harness::new();
    no_smart_titles(&harness);
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "Plan my garden");
    tui.wait_for_window_title("✦ Plan my garden");
    tui.command("/new");
    tui.wait_for_window_title("✦ sparkle");
    tui.quit();
}

// Thinking

#[test]
fn thinking_is_visible_by_default_and_toggles() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "hello");
    tui.wait_for("│ Thinking hard");
    tui.send(CTRL_T);
    tui.wait_gone("│ Thinking hard");
    tui.wait_for("Thought for");
    tui.send(CTRL_T);
    tui.wait_for("│ Thinking hard");
    tui.quit();
}

#[test]
fn hide_reasoning_flag_collapses_thinking_in_both_entry_points() {
    for args in [&["-R"][..], &["chat", "-R"][..]] {
        let harness = Harness::new();
        let mut tui = Tui::open(&harness, args);
        tui.ask(&harness, "hello");
        tui.wait_for("Hello world");
        assert!(!tui.screen().contains("│ Thinking hard"), "{args:?}\n{}", tui.screen());
        tui.quit();
    }
}

#[test]
fn show_thinking_setting_collapses_thinking() {
    let harness = Harness::new();
    harness.write_file("config.toml", "show_thinking = false\n");
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "hello");
    tui.wait_for("Hello world");
    assert!(!tui.screen().contains("│ Thinking hard"), "{}", tui.screen());
    tui.quit();
}

#[test]
fn hiding_thinking_with_ctrl_t_is_remembered_for_the_next_chat() {
    let harness = Harness::new();
    // The saved choice wins over the config, which only sets the starting point.
    harness.write_file("config.toml", "show_thinking = true\n");
    let mut tui = Tui::open(&harness, &[]);
    tui.send("\x14");
    tui.quit();

    let mut next = Tui::open(&harness, &[]);
    next.ask(&harness, "hello");
    next.wait_for("Hello world");
    assert!(!next.screen().contains("│ Thinking hard"), "{}", next.screen());
    next.send("\x14");
    next.wait_for("│ Thinking hard");
    next.quit();

    let mut third = Tui::open(&harness, &[]);
    third.ask(&harness, "hello");
    third.wait_for("│ Thinking hard");
    third.quit();
}

#[test]
fn the_chat_gives_the_model_a_journal_but_one_shot_prompts_do_not() {
    let harness = Harness::new();
    harness.write_file("config.toml", "journal = true\n");
    let mut tui = Tui::open(&harness, &["--system", "Be brief"]);
    tui.wait_for("Journal: on");
    tui.ask(&harness, "hello");
    tui.wait_for("Hello world");
    let system = harness.last_body()["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(system.starts_with("Be brief\n\nYou keep a journal"), "{system}");
    tui.quit();

    assert!(harness.run(&["--system", "Be brief", "hello"]).status.success());
    assert_eq!(harness.last_body()["messages"][0]["content"], "Be brief");

    harness.write_file("config.toml", "journal = false\n");
    let mut off = Tui::open(&harness, &["--system", "Be brief"]);
    off.ask(&harness, "hello");
    off.wait_for("Hello world");
    assert_eq!(harness.last_body()["messages"][0]["content"], "Be brief");
    off.quit();
}

// Leaving the chat

#[test]
fn a_single_exit_key_only_asks_for_confirmation() {
    for (key, name) in [(CTRL_C, "ctrl+c"), (CTRL_D, "ctrl+d")] {
        let harness = Harness::new();
        let mut tui = Tui::open(&harness, &[]);
        tui.send(key);
        tui.wait_for(&format!("Press {name} again to exit"));
        assert!(tui.is_running());
        tui.send(key);
        assert!(tui.wait_exit(), "{name}");
    }
}

#[test]
fn the_exit_prompt_expires_on_its_own() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send(CTRL_C);
    tui.wait_for("again to exit");
    tui.wait_gone("again to exit");
    tui.send(CTRL_C);
    tui.wait_for("again to exit");
    assert!(tui.is_running(), "a press after the prompt expired must not exit");
    tui.send(CTRL_C);
    assert!(tui.wait_exit());
}

#[test]
fn another_key_cancels_the_exit() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send(CTRL_D);
    tui.wait_for("again to exit");
    tui.send("x");
    tui.wait_gone("again to exit");
    tui.send(CTRL_C);
    tui.wait_gone("x  ");
    assert!(tui.is_running());
    tui.quit();
}

#[test]
fn quit_command_exits_immediately() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send("/quit\r");
    assert!(tui.wait_exit());
}

// Stopping replies

#[test]
fn escape_stops_a_reply_and_keeps_the_chat_open() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send("tell me slowly\r");
    tui.wait_for("esc to stop");
    tui.send(ESC);
    tui.wait_for("Stopped");
    assert!(tui.is_running());
    tui.quit();
}

#[test]
fn ctrl_c_stops_a_reply_without_exiting() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send("tell me slowly\r");
    tui.wait_for("esc to stop");
    tui.send(CTRL_C);
    tui.wait_for("Stopped");
    assert!(!tui.screen().contains("again to exit"));
    assert!(tui.is_running());
    tui.quit();
}

#[test]
fn server_errors_stay_in_the_chat_and_the_next_message_works() {
    let harness = Harness::new();
    no_smart_titles(&harness);
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "http500");
    tui.wait_for("mock exploded");
    tui.ask(&harness, "hello");
    tui.wait_for("Hello world");
    assert_eq!(contents(&harness.last_body()), ["http500\n\nhello"]);
    tui.quit();
}

#[test]
fn errors_in_the_middle_of_a_reply_keep_the_partial_text() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "midstream");
    tui.wait_for("stream broke");
    tui.wait_for("partial");
    tui.quit();
}

// Commands

#[test]
fn help_lists_every_command() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send("/help\r");
    tui.wait_for("Show commands and keys");
    for command in ["/new", "/model", "/effort", "/persona", "/system", "/prefill", "/retry", "/edit", "/compact", "/quit"] {
        tui.wait_for(command);
    }
    tui.quit();
}

#[test]
fn model_command_switches_the_model() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.command("/model mk2");
    tui.wait_for("Switched to mock2");
    tui.ask(&harness, "hello");
    assert_eq!(harness.last_body()["model"], "mock2-wire");
    tui.command("/model nope");
    tui.wait_for("unknown model 'nope'");
    tui.quit();
}

#[test]
fn effort_command_and_shift_tab_change_the_request() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.command("/effort high");
    tui.wait_for("Reasoning effort: high");
    tui.ask(&harness, "one");
    assert_eq!(harness.last_body()["reasoning_effort"], "high");
    tui.send(SHIFT_TAB);
    tui.wait_for("Reasoning effort: auto");
    tui.send(SHIFT_TAB);
    tui.wait_for("Reasoning effort: low");
    tui.ask(&harness, "two");
    assert_eq!(harness.last_body()["reasoning_effort"], "low");
    tui.command("/effort auto");
    tui.ask(&harness, "three");
    assert!(harness.last_body().get("reasoning_effort").is_none());
    tui.quit();
}

#[test]
fn persona_and_system_commands_change_the_system_prompt() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.command("/persona concise");
    tui.wait_for("Persona: concise");
    tui.ask(&harness, "one");
    assert!(contents(&harness.last_body())[0].starts_with("Answer directly"));
    tui.command("/system Speak like a pirate");
    tui.ask(&harness, "two");
    assert_eq!(contents(&harness.last_body())[0], "Speak like a pirate");
    tui.command("/persona off");
    tui.ask(&harness, "three");
    assert_eq!(harness.last_body()["messages"][0]["role"], "user");
    tui.command("/persona nope");
    tui.wait_for("Unknown persona 'nope'");
    tui.quit();
}

#[test]
fn the_prompt_browser_filters_and_applies_a_prompt() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.command("/prompts");
    tui.wait_for("System prompts");
    tui.send("gpt-3.5");
    tui.wait_for("ChatGPT GPT-3.5");
    tui.send("\r");
    tui.wait_gone("type to filter");
    tui.wait_for("System prompt: openai/gpt-3.5");
    tui.ask(&harness, "hello");
    assert!(contents(&harness.last_body())[0].contains("GPT-3.5"));
    tui.quit();
}

#[test]
fn the_prompt_browser_closes_without_changing_anything() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.command("/prompts");
    tui.wait_for("System prompts");
    tui.send("\x1b");
    tui.wait_gone("System prompts");
    tui.ask(&harness, "hello");
    assert_eq!(harness.last_body()["messages"][0]["role"], "user");
    tui.quit();
}

#[test]
fn the_prompt_browser_warns_about_prompts_too_large_for_the_model() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.wait_for("Context window: 8k tokens");
    tui.command("/prompts");
    tui.wait_for("System prompts");
    tui.send("xai/grok-4.7");
    tui.wait_for("Over the context limit");
    tui.send("\r");
    tui.wait_for("xai/grok-4.7: Over the context limit");
    tui.quit();
}

#[test]
fn a_default_chosen_in_the_browser_starts_the_next_chat() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.command("/prompts");
    tui.wait_for("System prompts");
    tui.send("gpt-3.5");
    tui.wait_for("ChatGPT GPT-3.5");
    tui.send("\x04");
    tui.wait_for("+");
    tui.send("\x1b");
    tui.wait_gone("System prompts");
    tui.quit();

    assert_eq!(stdout(&harness.run(&["prompts", "default"])).trim(), "openai/gpt-3.5");
    let mut next = Tui::open(&harness, &[]);
    next.wait_for("System prompt: openai/gpt-3.5");
    next.quit();
}

#[test]
fn the_prompt_browser_loads_an_external_file() {
    let harness = Harness::new();
    let path = harness.write_file("house-style.md", "Use the house style.");
    let mut tui = Tui::open(&harness, &[]);
    tui.command("/prompts");
    tui.wait_for("System prompts");
    tui.send("\x0f");
    tui.wait_for("File:");
    tui.send("/nonexistent/file.md\r");
    tui.wait_for("could not read");
    tui.send(&"\x7f".repeat("/nonexistent/file.md".len()));
    tui.send(&format!("{path}\r"));
    tui.wait_gone("System prompts");
    tui.wait_for("System prompt: house-style.md");
    tui.ask(&harness, "hello");
    assert_eq!(contents(&harness.last_body())[0], "Use the house style.");
    tui.quit();
}

#[test]
fn prompts_command_accepts_a_name_directly() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.command("/prompts anthropic/claude-3-opus");
    tui.wait_for("anthropic/claude-3-opus");
    tui.ask(&harness, "one");
    assert!(contents(&harness.last_body())[0].contains("Claude"));
    tui.command("/prompts off");
    tui.wait_for("System prompt cleared");
    tui.ask(&harness, "two");
    assert_eq!(harness.last_body()["messages"][0]["role"], "user");
    tui.command("/prompts nope");
    tui.wait_for("unknown system prompt 'nope'");
    tui.quit();
}

#[test]
fn prefill_command_applies_to_the_next_reply_only() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.command("/prefill Hello");
    tui.wait_for("prefill set");
    tui.ask(&harness, "one");
    assert_eq!(contents(&harness.last_body()).last().unwrap(), "Hello");
    tui.wait_for("Hello world");
    assert!(!tui.screen().contains("HelloHello"), "{}", tui.screen());
    tui.ask(&harness, "two");
    assert_eq!(contents(&harness.last_body()).last().unwrap(), "two");
    tui.quit();
}

#[test]
fn new_command_starts_an_empty_conversation() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "first topic");
    tui.command("/new");
    tui.wait_gone("first topic");
    tui.ask(&harness, "second topic");
    assert_eq!(contents(&harness.last_body()), ["second topic"]);
    tui.quit();
}

#[test]
fn retry_regenerates_the_reply() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "hello");
    tui.ask(&harness, "/retry");
    assert_eq!(harness.chat_requests().len(), 2);
    assert_eq!(contents(&harness.last_body()), ["hello"]);
    tui.quit();
}

#[test]
fn edit_opens_the_editor_and_the_next_request_uses_the_change() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::new();
    let editor = harness.write_file("editor.sh", "#!/bin/sh\nprintf 'EDITED REPLY\\n' > \"$1\"\n");
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut tui = Tui::spawn(&harness, &[], &[("EDITOR", &editor)]);
    tui.wait_for(READY);
    tui.ask(&harness, "hello");
    tui.wait_for("Hello world");
    tui.send("/edit\r");
    tui.wait_for("EDITED REPLY");
    tui.wait_for("Reply updated");
    tui.ask(&harness, "next");
    assert_eq!(contents(&harness.last_body())[1], "EDITED REPLY");
    tui.quit();
}

#[test]
fn a_failing_editor_leaves_the_reply_alone() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::new();
    let editor = harness.write_file("editor.sh", "#!/bin/sh\nexit 3\n");
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut tui = Tui::spawn(&harness, &[], &[("EDITOR", &editor)]);
    tui.wait_for(READY);
    tui.ask(&harness, "hello");
    tui.send("/edit\r");
    tui.wait_for("Could not edit");
    tui.wait_for("Hello world");
    tui.quit();
}

#[test]
fn unknown_commands_keep_the_typed_text() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send("/modle x\r");
    tui.wait_for("Unknown command /modle");
    tui.wait_for("/modle x");
    assert!(harness.chat_requests().is_empty());
    tui.quit_after_clearing();
}

// Compaction

const SUMMARY_MESSAGE_START: &str = "[Summary of the earlier conversation";

/// Opens a chat with two finished exchanges under the system prompt `Be brief`.
fn chat_with_two_exchanges(harness: &Harness) -> Tui {
    no_smart_titles(harness);
    let tui = Tui::open(harness, &["-s", "Be brief"]);
    tui.ask(harness, "first question");
    tui.ask(harness, "second question");
    tui
}

#[test]
fn compact_with_nothing_to_summarize_says_so() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send("/compact\r");
    tui.wait_for("Nothing to compact yet");
    assert!(harness.chat_requests().is_empty());
    tui.quit();
}

#[test]
fn compact_replaces_the_conversation_with_a_summary() {
    let harness = Harness::new();
    let mut tui = chat_with_two_exchanges(&harness);
    tui.ask(&harness, "/compact keep the names");

    let request = contents(&harness.last_body());
    assert_eq!(request[0], "Be brief");
    assert_eq!(request[1], "first question");
    assert!(request.last().unwrap().ends_with("focus on: keep the names"), "{request:?}");

    tui.wait_for("compacted (summary)");
    tui.wait_for("Compacted 4 messages via summary");
    // Prior messages stay visible in the scrollback above the divider, and ctrl+o expands summary.
    tui.send("\x0f");
    tui.wait_for(MOCK_SUMMARY);

    tui.ask(&harness, "third question");
    let sent = contents(&harness.last_body());
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0], "Be brief");
    assert!(sent[1].starts_with(SUMMARY_MESSAGE_START), "{sent:?}");
    assert!(sent[1].ends_with(&format!("{MOCK_SUMMARY}\n\nthird question")), "{sent:?}");
    tui.quit();
}

#[test]
fn compact_lowers_the_context_meter() {
    let harness = Harness::new();
    let mut tui = chat_with_two_exchanges(&harness);
    tui.wait_for("18/8k (0%)");
    tui.ask(&harness, "/compact");
    tui.wait_for("Compacted 4 messages");
    tui.wait_for("~24/8k (0%)");
    tui.quit();
}

#[test]
fn escape_stops_a_compaction_and_keeps_the_conversation() {
    for key in [ESC, CTRL_C] {
        let harness = Harness::new();
        let mut tui = chat_with_two_exchanges(&harness);
        tui.send("/compact slowly\r");
        tui.wait_for("Compacting");
        tui.send(key);
        tui.wait_for("Compaction stopped");
        assert!(tui.is_running());

        tui.ask(&harness, "third question");
        let sent = contents(&harness.last_body());
        assert_eq!(sent[1], "first question");
        assert_eq!(sent.last().unwrap(), "third question");
        tui.quit();
    }
}

#[test]
fn a_failed_compaction_keeps_the_conversation() {
    let harness = Harness::new();
    let mut tui = chat_with_two_exchanges(&harness);
    tui.ask(&harness, "/compact http500");
    tui.wait_for("Compaction failed");
    tui.wait_for("mock exploded");
    tui.ask(&harness, "third question");
    assert_eq!(contents(&harness.last_body())[1], "first question");
    tui.quit();
}

#[test]
fn a_compacted_chat_resumes_from_its_summary_in_every_front_end() {
    let harness = Harness::new();
    let mut tui = chat_with_two_exchanges(&harness);
    tui.ask(&harness, "/compact");
    tui.wait_for("Compacted 4 messages");
    tui.quit();

    let logs: Value = serde_json::from_str(&stdout(&harness.run(&["logs", "--json"]))).unwrap();
    assert_eq!(logs[0]["messages"], 6, "the original messages stay in the database");
    let id = logs[0]["id"].as_str().unwrap().to_string();

    let mut resumed = Tui::open(&harness, &["chat", "--cid", &id]);
    resumed.wait_for("compacted (summary)");
    resumed.send("\x0f");
    resumed.wait_for(MOCK_SUMMARY);
    assert!(!resumed.screen().contains(SUMMARY_MESSAGE_START), "{}", resumed.screen());
    resumed.ask(&harness, "third question");
    let sent = contents(&harness.last_body());
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0], "Be brief");
    assert!(sent[1].starts_with(SUMMARY_MESSAGE_START), "{sent:?}");
    resumed.quit();

    assert!(harness.run(&["-R", "-c", "fourth question"]).status.success());
    let sent = harness.last_body();
    let roles: Vec<_> = sent["messages"].as_array().unwrap().iter().map(|m| &m["role"]).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    let sent = contents(&sent);
    assert!(sent[1].contains("third question"), "{sent:?}");
    assert!(!sent.iter().any(|text| text == "first question"), "{sent:?}");
    assert_eq!(sent[3], "fourth question");
}

#[test]
fn a_one_shot_prompt_right_after_compaction_joins_the_summary() {
    let harness = Harness::new();
    let mut tui = chat_with_two_exchanges(&harness);
    tui.ask(&harness, "/compact");
    tui.wait_for("Compacted 4 messages");
    tui.quit();

    assert!(harness.run(&["-R", "-c", "next question"]).status.success());
    let body = harness.last_body();
    let roles: Vec<_> = body["messages"].as_array().unwrap().iter().map(|m| &m["role"]).collect();
    assert_eq!(roles, ["system", "user"], "strict servers reject two user messages in a row");
    let sent = contents(&body);
    assert!(sent[1].starts_with(SUMMARY_MESSAGE_START), "{sent:?}");
    assert!(sent[1].ends_with(&format!("{MOCK_SUMMARY}\n\nnext question")), "{sent:?}");
}

#[test]
fn compaction_works_in_a_chat_that_is_not_saved() {
    let harness = Harness::new();
    no_smart_titles(&harness);
    let mut tui = Tui::open(&harness, &["chat", "--no-log"]);
    tui.ask(&harness, "private one");
    tui.ask(&harness, "private two");
    tui.ask(&harness, "/compact");
    tui.wait_for("Compacted 4 messages");
    tui.ask(&harness, "private three");
    assert_eq!(contents(&harness.last_body()).len(), 1);
    tui.quit();
    assert_eq!(stdout(&harness.run(&["logs"])), "");
}

// Typing

#[test]
fn tab_completes_commands_end_to_end() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send("/mo");
    tui.wait_for("/mo");
    tui.send(TAB);
    tui.send("mk2\r");
    tui.wait_for("Switched to mock2");

    tui.send("/e");
    tui.wait_for("/effort  /edit");
    tui.send(TAB);
    tui.send(TAB);
    tui.send(" ");
    tui.send(CTRL_C);
    tui.quit();
}

#[test]
fn ctrl_j_and_paste_create_multiline_messages() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send("line one");
    tui.send(CTRL_J);
    tui.ask(&harness, "line two");
    assert_eq!(contents(&harness.last_body())[0], "line one\nline two");

    tui.send("\x1b[200~pasted one\npasted two\x1b[201~");
    tui.wait_for("pasted two");
    tui.ask(&harness, "");
    assert_eq!(contents(&harness.last_body()).last().unwrap(), "pasted one\npasted two");
    tui.quit();
}

#[test]
fn survives_resizing_while_replying() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.send("tell me slowly\r");
    tui.wait_for("esc to stop");
    for (rows, cols) in [(10, 30), (5, 12), (40, 140), (24, 80)] {
        tui.resize(rows, cols);
        std::thread::sleep(Duration::from_millis(120));
    }
    tui.wait_gone("esc to stop");
    tui.wait_for("Hello world");
    tui.quit();
}

#[test]
fn keys_pressed_right_after_a_resize_are_handled() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    for round in 0..12u16 {
        tui.resize(20 + round % 5, 70 + round);
        tui.ask(&harness, &format!("message {round}"));
    }
    assert_eq!(harness.chat_requests().len(), 12);
    tui.quit();
}

#[test]
fn scrolling_keys_move_through_a_long_transcript() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &[]);
    tui.resize(14, 80);
    for number in 0..4 {
        tui.ask(&harness, &format!("question {number}"));
    }
    tui.wait_gone("question 0");
    for _ in 0..8 {
        tui.send("\x1b[5~");
    }
    tui.wait_for("question 0");
    for _ in 0..8 {
        tui.send("\x1b[6~");
    }
    tui.wait_gone("question 0");
    tui.quit();
}

// History

#[test]
fn continue_and_cid_resume_a_conversation() {
    let harness = Harness::new();
    let output = harness.run(&["-R", "first question"]);
    assert!(output.status.success());
    let logs: Value = serde_json::from_str(&stdout(&harness.run(&["logs", "--json"]))).unwrap();
    let id = logs[0]["id"].as_str().unwrap().to_string();

    for (round, args) in [vec!["chat", "-c"], vec!["chat", "--cid", &id], vec!["-c"]].iter().enumerate() {
        let mut tui = Tui::open(&harness, args);
        for _ in 0..4 {
            tui.send("\x1b[5~");
        }
        tui.wait_for("first question");
        tui.ask(&harness, "follow up");
        let sent = contents(&harness.last_body());
        assert_eq!(sent.len(), 3 + round * 2, "{args:?} should resend the whole conversation");
        assert_eq!(sent[0], "first question", "{args:?}");
        assert_eq!(sent.last().unwrap(), "follow up", "{args:?}");
        tui.quit();
    }
}

#[test]
fn an_unknown_conversation_is_an_error() {
    let harness = Harness::new();
    let mut tui = Tui::spawn(&harness, &["chat", "--cid", "missing"], &[]);
    assert!(!tui.wait_exit());
    assert!(tui.raw().contains("no conversation found"), "{}", tui.raw());
}

#[test]
fn no_log_chats_are_not_saved() {
    let harness = Harness::new();
    let mut tui = Tui::open(&harness, &["chat", "--no-log"]);
    tui.ask(&harness, "private");
    tui.ask(&harness, "still private");
    assert_eq!(contents(&harness.last_body()).len(), 3);
    tui.quit();
    assert!(!tui.raw().contains("Continue this conversation"));
    assert_eq!(stdout(&harness.run(&["logs"])), "");
}

#[test]
fn chats_are_saved_with_their_replies() {
    let harness = Harness::new();
    no_smart_titles(&harness);
    let mut tui = Tui::open(&harness, &[]);
    tui.ask(&harness, "remember this");
    tui.quit();
    let logs: Value = serde_json::from_str(&stdout(&harness.run(&["logs", "--json"]))).unwrap();
    assert_eq!(logs[0]["title"], "remember this");
    assert_eq!(logs[0]["messages"], 2);
}

// Appearance

#[test]
fn icons_follow_the_terminal_and_the_setting() {
    let harness = Harness::new();
    let mut nerd = Tui::spawn(&harness, &[], &[("TERM", "xterm-ghostty")]);
    nerd.wait_for("\u{f0674}");
    nerd.quit();

    harness.write_file("config.toml", "icons = \"ascii\"\n");
    let mut ascii = Tui::spawn(&harness, &[], &[("TERM", "xterm-ghostty")]);
    ascii.wait_for("*  sparkle");
    ascii.wait_for("Chatting with mock");
    ascii.quit();
}

#[test]
fn reduced_motion_replaces_the_spinner() {
    let harness = Harness::new();
    let mut tui = Tui::spawn(&harness, &[], &[("SPARKLE_REDUCED_MOTION", "1")]);
    tui.wait_for(READY);
    tui.send("tell me slowly\r");
    tui.wait_for("esc to stop");
    assert!(!["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"].iter().any(|frame| tui.raw().contains(frame)));
    tui.send(ESC);
    tui.wait_for("Stopped");
    tui.quit();
}

#[test]
fn an_invalid_config_is_reported_before_the_screen_opens() {
    let harness = Harness::new();
    harness.write_file("config.toml", "nonsense = true\n");
    let mut tui = Tui::spawn(&harness, &[], &[]);
    assert!(!tui.wait_exit());
    assert!(tui.raw().contains("config.toml"), "{}", tui.raw());
    assert!(!tui.raw().contains("\x1b[?1049h"), "the alternate screen should not open");
}

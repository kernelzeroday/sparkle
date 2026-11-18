//! End-to-end tests: the real binary talking to a local OpenAI-compatible mock server.

mod common;

use common::*;
use serde_json::{Value, json};

#[test]
fn reply_goes_to_stdout_and_reasoning_to_stderr() {
    let harness = Harness::new();
    let output = harness.run(&["hello"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{REPLY}\n"));
    assert!(stderr(&output).contains("Thinking hard"));
    let recorded = harness.requests.lock().unwrap()[0].clone();
    assert_eq!(recorded.body["model"], "mock-wire");
    assert_eq!(recorded.authorization.as_deref(), Some("Bearer sk-test"));
}

#[test]
fn hide_reasoning_keeps_stderr_clean_when_piped() {
    let output = Harness::new().run(&["-R", "hello"]);
    assert_eq!(stderr(&output), "");
}

#[test]
fn friendly_flags_reach_the_wire() {
    let harness = Harness::new();
    harness.run(&[
        "--effort", "high", "--temp", "0.3", "--max-tokens", "9", "-o", "verbosity", "low", "-o", "top_k", "4", "hi",
    ]);
    let body = harness.last_body();
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["temperature"], 0.3);
    assert_eq!(body["max_tokens"], 9);
    assert_eq!(body["verbosity"], "low");
    assert_eq!(body["top_k"], 4);
}

#[test]
fn persona_sets_the_system_prompt_and_system_flag_wins() {
    let harness = Harness::new();
    harness.run(&["--persona", "concise", "hi"]);
    assert!(harness.last_body()["messages"][0]["content"].as_str().unwrap().starts_with("Answer directly"));
    harness.run(&["--persona", "concise", "-s", "Mine", "hi"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "Mine");
    let output = harness.run(&["--persona", "nope", "hi"]);
    assert!(stderr(&output).contains("unknown persona"));
}

#[test]
fn repeated_system_fragments_are_joined_before_the_system_prompt() {
    let harness = Harness::new();
    let first = harness.home.join("a.txt");
    let second = harness.home.join("b.txt");
    std::fs::write(&first, "ALPHA\n").unwrap();
    std::fs::write(&second, "BETA").unwrap();
    let (first, second) = (first.to_str().unwrap(), second.to_str().unwrap());

    harness.run(&["-R", "--sf", first, "--sf", second, "hi"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "ALPHA\n\nBETA");

    harness.run(&["-R", "--sf", first, "--system-fragment", second, "-s", "SYS", "hi"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "ALPHA\n\nBETA\n\nSYS");

    harness.run(&["-R", "--sf", first, "--persona", "concise", "hi"]);
    let system = harness.last_body()["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(system.starts_with("ALPHA\n\nAnswer directly"), "{system}");
}

#[test]
fn continuing_with_the_same_system_fragment_does_not_repeat_it() {
    let harness = Harness::new();
    let path = harness.home.join("a.txt");
    std::fs::write(&path, "ALPHA\n").unwrap();
    let path = path.to_str().unwrap();
    harness.run(&["-R", "--sf", path, "first"]);
    harness.run(&["-R", "-c", "--sf", path, "second"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "ALPHA");
    harness.run(&["-R", "-c", "third"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "ALPHA");
}

#[test]
fn plain_chat_accepts_system_fragments() {
    let harness = Harness::new();
    let path = harness.home.join("a.txt");
    std::fs::write(&path, "ALPHA").unwrap();
    let args = ["chat", "--plain", "-R", "--sf", path.to_str().unwrap()];
    let output = harness.run_with_stdin(&args, "hello\nquit\n");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(harness.last_body()["messages"][0]["content"], "ALPHA");
}

#[test]
fn show_thinking_setting_controls_reasoning_output() {
    let harness = Harness::new();
    assert!(stderr(&harness.run(&["hello"])).contains("Thinking hard"));

    std::fs::write(harness.home.join("config.toml"), "show_thinking = false\n").unwrap();
    let output = harness.run(&["hello"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    assert_eq!(stdout(&output), format!("{REPLY}\n"));

    let chat = harness.run_with_stdin(&["chat", "--plain"], "hello\nquit\n");
    assert!(!stderr(&chat).contains("Thinking hard"), "{}", stderr(&chat));
}

#[test]
fn one_shot_fragments_come_before_the_prompt() {
    let harness = Harness::new();
    let path = harness.home.join("doc.txt");
    std::fs::write(&path, "FILE\n").unwrap();
    harness.run(&["-R", "-f", path.to_str().unwrap(), "question"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "FILE\n\nquestion");
}

#[test]
fn plain_chat_sends_fragments_with_the_first_message_only() {
    for extra in [&[][..], &["-n"][..]] {
        let harness = Harness::new();
        let path = harness.home.join("doc.txt");
        std::fs::write(&path, "FILE\n").unwrap();
        let mut args = vec!["chat", "--plain", "-R", "-f", path.to_str().unwrap()];
        args.extend_from_slice(extra);
        let output = harness.run_with_stdin(&args, "first\nsecond\nquit\n");
        assert!(output.status.success(), "{}", stderr(&output));

        let messages = harness.last_body()["messages"].as_array().unwrap().clone();
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert_eq!(messages[0]["content"], "FILE\n\nfirst");
        assert_eq!(messages[2]["content"], "second");
    }
}

#[test]
fn plain_chat_resends_fragments_after_a_failed_first_message() {
    let harness = Harness::new();
    let path = harness.home.join("doc.txt");
    std::fs::write(&path, "FILE\n").unwrap();
    let args = ["chat", "--plain", "-R", "-f", path.to_str().unwrap()];
    harness.run_with_stdin(&args, "http500\nhello\nquit\n");
    assert_eq!(harness.last_body()["messages"][0]["content"], "FILE\n\nhello");
}

#[test]
fn missing_fragment_is_an_error_in_chat() {
    let output = Harness::new().run(&["chat", "--plain", "-f", "/nonexistent/doc.md"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("could not read fragment"), "{}", stderr(&output));
}

#[test]
fn missing_system_fragment_is_an_error() {
    let harness = Harness::new();
    let output = harness.run(&["--sf", "/nonexistent/prompt.md", "hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("could not read system fragment"), "{}", stderr(&output));
    assert!(harness.requests.lock().unwrap().is_empty());
}

#[test]
fn prefill_is_sent_and_printed_first() {
    let harness = Harness::new();
    let output = harness.run(&["-R", "--prefill", "Sure: ", "hi"]);
    assert!(stdout(&output).starts_with("Sure: Hello world"));
    let body = harness.last_body();
    assert_eq!(body["messages"][1], json!({ "role": "assistant", "content": "Sure: " }));
}

#[test]
fn extract_prints_first_and_last_blocks() {
    let harness = Harness::new();
    assert_eq!(stdout(&harness.run(&["-R", "-x", "hi"])), "print('hi')\n");
    assert_eq!(stdout(&harness.run(&["-R", "--xl", "hi"])), "ls\n");
}

#[test]
fn piped_input_comes_before_the_prompt() {
    let harness = Harness::new();
    harness.run_with_stdin(&["-R", "ARG"], "PIPED\n");
    assert_eq!(harness.last_body()["messages"][0]["content"], "PIPED\n ARG");
}

#[test]
fn continue_sends_the_earlier_conversation() {
    let harness = Harness::new();
    harness.run(&["-R", "-s", "SYS", "first"]);
    harness.run(&["-R", "-c", "second"]);
    let messages = harness.last_body()["messages"].as_array().unwrap().clone();
    let roles: Vec<_> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(messages[0]["content"], "SYS");
    assert_eq!(messages[2]["content"], REPLY);
    assert_eq!(harness.last_body()["model"], "mock-wire");
}

#[test]
fn no_log_leaves_nothing_to_continue() {
    let harness = Harness::new();
    harness.run(&["-R", "-n", "secret"]);
    let output = harness.run(&["-c", "again"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("no conversation to continue"));
}

#[test]
fn schema_dsl_becomes_response_format() {
    let harness = Harness::new();
    harness.run(&["-R", "--schema", "name, age int", "dog"]);
    let format = &harness.last_body()["response_format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(format["json_schema"]["name"], "output");
    assert_eq!(format["json_schema"]["schema"]["properties"]["age"]["type"], "integer");
}

#[test]
fn json_output_includes_reply_and_usage() {
    let output = Harness::new().run(&["--json", "hello"]);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value[0]["response"], REPLY);
    assert_eq!(value[0]["reasoning"], "Thinking hard");
    assert_eq!(value[0]["input_tokens"], 11);
    assert_eq!(value[0]["model"], "mock");
}

#[test]
fn no_stream_uses_a_single_response() {
    let harness = Harness::new();
    let output = harness.run(&["--no-stream", "-u", "hello"]);
    assert_eq!(stdout(&output), "whole reply\n");
    assert!(stderr(&output).contains("Token usage: 2 input, 3 output"));
    assert_eq!(harness.last_body()["stream"], false);
    assert!(harness.last_body().get("stream_options").is_none());
}

#[test]
fn attachments_are_sent_inline() {
    let harness = Harness::new();
    let path = harness.home.join("a.png");
    std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
    harness.run(&["-R", "-a", path.to_str().unwrap(), "see"]);
    assert_eq!(
        harness.last_body()["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
}

#[test]
fn server_errors_exit_nonzero_with_the_message() {
    let output = Harness::new().run(&["http500"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("mock exploded"), "{}", stderr(&output));
}

#[test]
fn unknown_models_are_explained() {
    let output = Harness::new().run(&["-m", "nope", "hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("unknown model 'nope'"));
}

#[test]
fn plain_chat_keeps_context_and_supports_multiline() {
    let harness = Harness::new();
    let output =
        harness.run_with_stdin(&["chat", "--plain", "-R"], "first\n!multi\nline a\nline b\n!end\nquit\n");
    assert!(output.status.success(), "{}", stderr(&output));
    let messages = harness.last_body()["messages"].as_array().unwrap().clone();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2]["content"], "line a\nline b");
}

#[test]
fn plain_chat_survives_a_failed_request() {
    let harness = Harness::new();
    let output = harness.run_with_stdin(&["chat", "--plain", "-R"], "http500\nhello\nquit\n");
    assert!(output.status.success());
    assert!(stderr(&output).contains("mock exploded"));
    assert!(stdout(&output).contains("Hello world"));
}

#[test]
fn help_exposes_effort_levels_and_models_list_works() {
    let harness = Harness::new();
    let help = stdout(&harness.run(&["--help"]));
    assert!(help.contains("--effort"));
    assert!(help.contains("high"));
    let models = stdout(&harness.run(&["models"]));
    assert!(models.contains("* mock (aliases: mk)"), "{models}");
}

#[test]
fn cid_continues_a_specific_conversation() {
    let harness = Harness::new();
    harness.run(&["-R", "topic one"]);
    let logs: Value = serde_json::from_str(&stdout(&harness.run(&["logs", "--json"]))).unwrap();
    let first = logs[0]["id"].as_str().unwrap().to_string();
    std::thread::sleep(std::time::Duration::from_millis(3));
    harness.run(&["-R", "topic two"]);

    harness.run(&["-R", "--cid", &first, "again"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "topic one");

    let output = harness.run(&["--cid", "missing", "hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no conversation found"));
}

#[test]
fn schema_multi_wraps_items_and_json_schema_is_accepted() {
    let harness = Harness::new();
    harness.run(&["-R", "--schema-multi", "name", "dogs"]);
    let schema = &harness.last_body()["response_format"]["json_schema"]["schema"];
    assert_eq!(schema["properties"]["items"]["items"]["properties"]["name"]["type"], "string");

    harness.run(&["-R", "--schema", r#"{"type":"object","title":"inline"}"#, "dog"]);
    assert_eq!(harness.last_body()["response_format"]["json_schema"]["schema"]["title"], "inline");

    let file = harness.write_file("schema.json", r#"{"type":"object","title":"file"}"#);
    harness.run(&["-R", "--schema", &file, "dog"]);
    assert_eq!(harness.last_body()["response_format"]["json_schema"]["schema"]["title"], "file");

    let output = harness.run(&["--schema", "age integer", "dog"]);
    assert!(stderr(&output).contains("unknown schema type"));
}

#[test]
fn typed_attachments_use_the_given_type() {
    let harness = Harness::new();
    let path = harness.write_file("data.bin", "x");
    harness.run(&["-R", "--at", &path, "image/webp", "see"]);
    let url = harness.last_body()["messages"][0]["content"][1]["image_url"]["url"].clone();
    assert_eq!(url, "data:image/webp;base64,eA==");
}

#[test]
fn piped_attachments_detect_their_type() {
    let harness = Harness::new();
    harness.run_with_bytes(&["-R", "-a", "-", "see"], b"\x89PNG\r\n\x1a\n");
    let body = harness.last_body();
    assert_eq!(body["messages"][0]["content"][0]["text"], "see");
    assert_eq!(body["messages"][0]["content"][1]["image_url"]["url"], "data:image/png;base64,iVBORw0KGgo=");

    harness.run_with_bytes(&["-R", "-a", "-", "read"], b"%PDF-1.7");
    assert_eq!(harness.last_body()["messages"][0]["content"][1]["type"], "file");
}

#[test]
fn url_attachments_are_passed_through() {
    let harness = Harness::new();
    harness.run(&["-R", "-a", "https://example.com/a.png", "see"]);
    let url = harness.last_body()["messages"][0]["content"][1]["image_url"]["url"].clone();
    assert_eq!(url, "https://example.com/a.png");
}

#[test]
fn missing_attachment_and_fragment_files_are_errors() {
    let harness = Harness::new();
    for flag in ["-a", "-f"] {
        let output = harness.run(&[flag, "/nonexistent/file", "hi"]);
        assert_eq!(output.status.code(), Some(1), "{flag}");
        assert!(stderr(&output).contains("/nonexistent/file"), "{flag}");
    }
    assert!(harness.chat_requests().is_empty());
}

#[test]
fn an_empty_prompt_is_explained() {
    let output = Harness::new().run(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no prompt given"));
}

#[test]
fn logs_list_conversations_newest_first() {
    let harness = Harness::new();
    assert_eq!(stdout(&harness.run(&["logs"])), "");
    for prompt in ["alpha", "beta", "gamma"] {
        harness.run(&["-R", prompt]);
        std::thread::sleep(std::time::Duration::from_millis(3));
    }
    let text = stdout(&harness.run(&["logs", "-n", "2"]));
    assert_eq!(text.lines().count(), 2);
    assert!(text.lines().next().unwrap().ends_with("gamma"), "{text}");

    assert_eq!(stdout(&harness.run(&["logs", "-n", "0"])).lines().count(), 3);
    let rows: Value = serde_json::from_str(&stdout(&harness.run(&["logs", "--json"]))).unwrap();
    assert_eq!(rows[2]["title"], "alpha");
    assert_eq!(rows[2]["model"], "mock");
    assert_eq!(rows[2]["messages"], 2);
}

#[test]
fn models_can_be_listed_as_json() {
    let harness = Harness::new();
    let rows: Value = serde_json::from_str(&stdout(&harness.run(&["models", "--json"]))).unwrap();
    assert_eq!(rows[0]["id"], "mock");
    assert_eq!(rows[0]["default"], true);
    assert_eq!(rows[1]["aliases"], json!(["mk2"]));
    assert_eq!(rows[1]["default"], false);
}

#[test]
fn personas_are_listed() {
    let text = stdout(&Harness::new().run(&["personas"]));
    for name in ["concise", "code", "explain", "editor", "shell", "socratic"] {
        assert!(text.contains(name), "{name}");
    }
}

#[test]
fn the_prompt_library_is_listed_and_printed() {
    let harness = Harness::new();
    let text = stdout(&harness.run(&["prompts"]));
    for id in ["openai/gpt-3.5", "openai/gpt-4o-2024-05", "anthropic/claude-opus-5.5", "xai/grok-1"] {
        assert!(text.contains(id), "{id}");
    }
    let rows: Value = serde_json::from_str(&stdout(&harness.run(&["prompts", "list", "--json"]))).unwrap();
    assert!(rows.as_array().unwrap().len() > 100);
    assert_eq!(rows[0]["family"], "OpenAI");

    let shown = stdout(&harness.run(&["prompts", "show", "openai/gpt-3.5"]));
    assert!(shown.starts_with("You are ChatGPT"), "{shown}");
}

#[test]
fn a_library_prompt_or_a_file_can_be_the_system_prompt() {
    let harness = Harness::new();
    harness.run(&["-S", "openai/gpt-3.5", "hi"]);
    let system = harness.last_body()["messages"][0].clone();
    assert_eq!(system["role"], "system");
    assert!(system["content"].as_str().unwrap().contains("GPT-3.5"));

    let path = harness.write_file("house-style.md", "Use the house style.");
    harness.run(&["--system-prompt", &path, "hi"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "Use the house style.");
}

#[test]
fn system_fragments_still_combine_with_a_library_prompt() {
    let harness = Harness::new();
    let path = harness.write_file("extra.md", "EXTRA RULES");
    harness.run(&["-S", "openai/gpt-3.5", "--sf", &path, "hi"]);
    let system = harness.last_body()["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(system.starts_with("EXTRA RULES\n\nYou are ChatGPT"), "{system}");
}

#[test]
fn an_unknown_system_prompt_is_an_error_before_any_request() {
    let harness = Harness::new();
    let output = harness.run(&["-S", "no-such-prompt", "hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("unknown system prompt"), "{}", stderr(&output));
    assert!(harness.requests.lock().unwrap().is_empty());
}

#[test]
fn the_default_prompt_applies_until_something_overrides_it() {
    let harness = Harness::new();
    assert!(stdout(&harness.run(&["prompts", "default"])).contains("No default"));
    let saved = stdout(&harness.run(&["prompts", "default", "gpt-3.5"]));
    assert!(saved.contains("openai/gpt-3.5"), "{saved}");
    assert_eq!(stdout(&harness.run(&["prompts", "default"])).trim(), "openai/gpt-3.5");

    harness.run(&["hi"]);
    let first = harness.last_body()["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(first.contains("GPT-3.5"), "{first}");

    harness.run(&["-s", "Explicit", "hi"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "Explicit");
    harness.run(&["--persona", "concise", "hi"]);
    let persona = harness.last_body()["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(persona.starts_with("Answer directly"), "{persona}");

    harness.run(&["prompts", "default", "off"]);
    harness.run(&["hi"]);
    assert_eq!(harness.last_body()["messages"][0]["role"], "user");
}

#[test]
fn a_default_prompt_file_that_disappeared_only_warns() {
    let harness = Harness::new();
    let path = harness.write_file("gone.md", "Soon gone");
    harness.run(&["prompts", "default", &path]);
    harness.run(&["hi"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "Soon gone");

    std::fs::remove_file(&path).unwrap();
    let output = harness.run(&["hi"]);
    assert!(output.status.success());
    assert!(stderr(&output).contains("ignoring the default system prompt"), "{}", stderr(&output));
    assert_eq!(harness.last_body()["messages"][0]["role"], "user");
}

#[test]
fn continuing_a_conversation_keeps_its_own_system_prompt() {
    let harness = Harness::new();
    harness.run(&["-s", "Original", "hi"]);
    harness.run(&["prompts", "default", "gpt-3.5"]);
    harness.run(&["-c", "again"]);
    assert_eq!(harness.last_body()["messages"][0]["content"], "Original");
}

#[test]
fn prompts_near_or_over_the_context_window_warn_on_stderr() {
    let harness = Harness::new();
    let quiet = harness.run(&["-S", "openai/gpt-3.5", "hi"]);
    assert!(!stderr(&quiet).contains("context limit"), "{}", stderr(&quiet));

    // The mock server reports a window of 8000 tokens.
    let near = harness.run(&["-S", "anthropic/claude-haiku-5.5", "hi"]);
    assert!(stderr(&near).contains("Close to the context limit"), "{}", stderr(&near));
    assert!(!stdout(&near).contains("context limit"));

    let over = harness.run(&["-S", "xai/grok-4.7", "hi"]);
    assert!(stderr(&over).contains("Over the context limit"), "{}", stderr(&over));
    assert_eq!(harness.chat_requests().len(), 3, "a warning does not stop the request");
}

#[test]
fn completions_and_man_page_are_generated() {
    let harness = Harness::new();
    for shell in ["bash", "zsh", "fish"] {
        let script = stdout(&harness.run(&["completions", shell]));
        assert!(script.contains("effort"), "{shell}");
    }
    assert!(stdout(&harness.run(&["man"])).contains(".TH sparkle"));
}

#[test]
fn config_file_supplies_defaults_that_flags_override() {
    let harness = Harness::new();
    harness.write_file(
        "config.toml",
        "default_model = \"mock2\"\neffort = \"low\"\npersona = \"concise\"\n",
    );
    harness.run(&["-R", "hi"]);
    let body = harness.last_body();
    assert_eq!(body["model"], "mock2-wire");
    assert_eq!(body["reasoning_effort"], "low");
    assert!(body["messages"][0]["content"].as_str().unwrap().starts_with("Answer directly"));

    harness.run(&["-R", "-m", "mock", "--effort", "high", "-s", "Mine", "hi"]);
    let body = harness.last_body();
    assert_eq!(body["model"], "mock-wire");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["messages"][0]["content"], "Mine");
}

#[test]
fn config_file_can_define_models_with_their_own_key() {
    let harness = Harness::new();
    let yaml = std::fs::read_to_string(harness.home.join("extra-openai-models.yaml")).unwrap();
    let base = yaml.lines().find_map(|line| line.trim().strip_prefix("api_base: ")).unwrap();
    harness.write_file(
        "config.toml",
        &format!("[[models]]\nid = \"own\"\nname = \"own-wire\"\napi_base = \"{base}\"\nkey_env = \"SPARKLE_TEST_KEY\"\naliases = [\"o1x\"]\n"),
    );
    let output = harness.command(&["-R", "-m", "o1x", "hi"]).env("SPARKLE_TEST_KEY", "sk-env").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let request = harness.last_request();
    assert_eq!(request.body["model"], "own-wire");
    assert_eq!(request.authorization.as_deref(), Some("Bearer sk-env"));
}

#[test]
fn an_invalid_config_file_names_the_file() {
    let harness = Harness::new();
    harness.write_file("config.toml", "nonsense = true\n");
    let output = harness.run(&["hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("config.toml"), "{}", stderr(&output));
}

#[test]
fn llm_aliases_and_stored_key_names_are_honored() {
    let harness = Harness::new();
    harness.write_file("aliases.json", r#"{"fast":"mock2"}"#);
    harness.run(&["-R", "-m", "fast", "--key", "mockkey", "hi"]);
    let request = harness.last_request();
    assert_eq!(request.body["model"], "mock2-wire");
    assert_eq!(request.authorization.as_deref(), Some("Bearer sk-test"));
}

#[test]
fn errors_in_the_middle_of_a_stream_fail_after_the_partial_text() {
    let output = Harness::new().run(&["-R", "midstream"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "partial ");
    assert!(stderr(&output).contains("stream broke"));
}

#[test]
fn a_closed_pipe_does_not_crash() {
    let harness = Harness::new();
    let mut child = harness
        .command(&["-R", "hello"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert!(!stderr(&output).contains("panicked"), "{}", stderr(&output));
}

#[test]
fn a_repeated_prefill_is_removed_from_output_and_history() {
    let harness = Harness::new();
    let output = harness.run(&["-R", "--prefill", "Hello", "hi"]);
    assert!(stdout(&output).starts_with("Hello world"), "{}", stdout(&output));
    harness.run(&["-R", "-c", "more"]);
    let reply = harness.last_body()["messages"][1]["content"].as_str().unwrap().to_string();
    assert!(reply.starts_with("Hello world"), "{reply}");
}

#[test]
fn plain_chat_prefills_only_the_first_reply() {
    let harness = Harness::new();
    let output = harness.run_with_stdin(&["chat", "--plain", "-R", "--prefill", "Sure: "], "one\ntwo\nquit\n");
    assert!(stdout(&output).starts_with("Sure: Hello world"), "{}", stdout(&output));
    let requests = harness.chat_requests();
    assert_eq!(requests[0].body["messages"][1]["content"], "Sure: ");
    let second = requests[1].body["messages"].as_array().unwrap();
    assert_eq!(second.last().unwrap()["content"], "two");
    assert!(second[1]["content"].as_str().unwrap().starts_with("Sure: Hello"));
}

#[test]
fn plain_chat_without_logging_still_remembers_the_conversation() {
    let harness = Harness::new();
    harness.run_with_stdin(&["chat", "--plain", "-R", "-n"], "one\ntwo\nquit\n");
    assert_eq!(harness.last_body()["messages"].as_array().unwrap().len(), 3);
    assert_eq!(stdout(&harness.run(&["logs"])), "");
}

#[test]
fn plain_chat_ends_at_end_of_input_and_ignores_blank_lines() {
    let harness = Harness::new();
    let output = harness.run_with_stdin(&["chat", "--plain", "-R"], "\n\nhello\n");
    assert!(output.status.success());
    assert_eq!(harness.chat_requests().len(), 1);
}

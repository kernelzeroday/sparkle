//! Every flag must work in every mode that accepts it. The guard tests at the bottom fail when a
//! new flag is added without being placed in one of these tables.

mod common;

use common::*;
use serde_json::json;

type Build = fn(&Harness) -> Vec<String>;
type Check = fn(&Recorded);

struct Case {
    flag: &'static str,
    args: Build,
    check: Check,
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// Flags shared by one-shot prompts and both kinds of chat.
const SHARED: &[Case] = &[
    Case {
        flag: "--model",
        args: |_| strings(&["--model", "mk2"]),
        check: |r| assert_eq!(r.body["model"], "mock2-wire"),
    },
    Case {
        flag: "--effort",
        args: |_| strings(&["--effort", "high"]),
        check: |r| assert_eq!(r.body["reasoning_effort"], "high"),
    },
    Case {
        flag: "--temp",
        args: |_| strings(&["--temp", "0.25"]),
        check: |r| assert_eq!(r.body["temperature"], 0.25),
    },
    Case {
        flag: "--top-p",
        args: |_| strings(&["--top-p", "0.5"]),
        check: |r| assert_eq!(r.body["top_p"], 0.5),
    },
    Case {
        flag: "--max-tokens",
        args: |_| strings(&["--max-tokens", "77"]),
        check: |r| assert_eq!(r.body["max_tokens"], 77),
    },
    Case {
        flag: "--seed",
        args: |_| strings(&["--seed", "9"]),
        check: |r| assert_eq!(r.body["seed"], 9),
    },
    Case {
        flag: "--option",
        args: |_| strings(&["--option", "top_k", "4"]),
        check: |r| assert_eq!(r.body["top_k"], 4),
    },
    Case {
        flag: "--key",
        args: |_| strings(&["--key", "sk-literal"]),
        check: |r| assert_eq!(r.authorization.as_deref(), Some("Bearer sk-literal")),
    },
    Case {
        flag: "--system",
        args: |_| strings(&["--system", "Be brief"]),
        check: |r| assert_eq!(r.body["messages"][0], json!({ "role": "system", "content": "Be brief" })),
    },
    Case {
        flag: "--sf",
        args: |h| vec!["--sf".into(), h.write_file("rules.md", "RULES\n")],
        check: |r| assert_eq!(r.body["messages"][0], json!({ "role": "system", "content": "RULES" })),
    },
    Case {
        flag: "--fragment",
        args: |h| vec!["--fragment".into(), h.write_file("doc.md", "DOCUMENT\n")],
        check: |r| assert_eq!(r.body["messages"][0]["content"], "DOCUMENT\n\nhello"),
    },
    Case {
        flag: "--persona",
        args: |_| strings(&["--persona", "concise"]),
        check: |r| {
            let system = r.body["messages"][0]["content"].as_str().unwrap();
            assert!(system.starts_with("Answer directly"), "{system}");
        },
    },
    Case {
        flag: "--system-prompt",
        args: |_| strings(&["--system-prompt", "openai/gpt-3.5"]),
        check: |r| {
            let system = r.body["messages"][0]["content"].as_str().unwrap();
            assert!(system.contains("GPT-3.5"), "{system}");
        },
    },
    Case {
        flag: "--prefill",
        args: |_| strings(&["--prefill", "Sure"]),
        check: |r| {
            let messages = r.body["messages"].as_array().unwrap();
            assert_eq!(messages.last().unwrap(), &json!({ "role": "assistant", "content": "Sure" }));
        },
    },
];

fn run_in_chat(harness: &Harness, leading: &[&str], args: &[String]) {
    let mut all: Vec<&str> = leading.to_vec();
    all.extend(args.iter().map(String::as_str));
    let mut tui = Tui::open(harness, &all);
    tui.ask(harness, "hello");
    tui.quit();
}

#[test]
fn shared_flags_work_in_one_shot_prompts() {
    for case in SHARED {
        let harness = Harness::new();
        let args = (case.args)(&harness);
        let mut all: Vec<&str> = args.iter().map(String::as_str).collect();
        all.push("hello");
        let output = harness.run(&all);
        assert!(output.status.success(), "{}: {}", case.flag, stderr(&output));
        (case.check)(&harness.last_request());
    }
}

#[test]
fn shared_flags_work_in_plain_chat() {
    for case in SHARED {
        let harness = Harness::new();
        let args = (case.args)(&harness);
        let mut all = vec!["chat", "--plain"];
        all.extend(args.iter().map(String::as_str));
        let output = harness.run_with_stdin(&all, "hello\nquit\n");
        assert!(output.status.success(), "{}: {}", case.flag, stderr(&output));
        (case.check)(&harness.last_request());
    }
}

#[test]
fn shared_flags_work_in_sparkle_chat() {
    for case in SHARED {
        let harness = Harness::new();
        run_in_chat(&harness, &["chat"], &(case.args)(&harness));
        (case.check)(&harness.last_request());
    }
}

#[test]
fn shared_flags_work_when_sparkle_opens_the_chat_by_itself() {
    for case in SHARED {
        let harness = Harness::new();
        run_in_chat(&harness, &[], &(case.args)(&harness));
        (case.check)(&harness.last_request());
    }
}

/// Flags that only mean something for a single prompt. With no prompt, in a terminal, they must
/// explain the problem rather than open a chat and ignore the flag.
const ONE_SHOT_ONLY: &[&[&str]] = &[
    &["--schema", "name"],
    &["--schema-multi", "name"],
    &["--no-stream"],
    &["--usage"],
    &["--extract"],
    &["--xl"],
    &["--json"],
];

#[test]
fn one_shot_flags_without_a_prompt_explain_themselves() {
    for args in ONE_SHOT_ONLY {
        let harness = Harness::new();
        let mut tui = Tui::spawn(&harness, args, &[]);
        assert!(!tui.wait_exit(), "{args:?} should fail");
        assert!(tui.raw().contains("no prompt given"), "{args:?}: {}", tui.raw());
        assert!(harness.chat_requests().is_empty());
    }
}

#[test]
fn attachments_without_a_prompt_are_sent_from_a_terminal() {
    let harness = Harness::new();
    let image = harness.write_file("a.png", "\u{1}");
    for args in [vec!["--attachment", &image], vec!["--at", &image, "image/png"]] {
        let mut tui = Tui::spawn(&harness, &args, &[]);
        assert!(tui.wait_exit(), "{args:?}: {}", tui.raw());
        let content = &harness.last_body()["messages"][0]["content"];
        assert_eq!(content[1]["type"], "image_url", "{args:?}");
    }
}

/// Covered by dedicated tests in `tui.rs` and `cli.rs`.
const DEDICATED: &[&str] = &[
    "--continue",
    "--cid",
    "--no-log",
    "--plain",
    "--hide-reasoning",
    "--help",
    "--version",
    "--attachment",
    "--at",
    "--system-fragment",
];

fn long_flags(help: &str) -> Vec<String> {
    let mut flags: Vec<String> = help
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-'))
        .filter(|word| word.starts_with("--") && word.len() > 2)
        .map(str::to_string)
        .collect();
    flags.sort();
    flags.dedup();
    flags
}

fn assert_all_classified(help_args: &[&str]) {
    let harness = Harness::new();
    let help = stdout(&harness.run(help_args));
    let flags = long_flags(&help);
    assert!(flags.len() > 10, "help should list the flags:\n{help}");
    for flag in flags {
        let known = SHARED.iter().any(|case| case.flag == flag)
            || ONE_SHOT_ONLY.iter().any(|args| args[0] == flag)
            || DEDICATED.contains(&flag.as_str());
        assert!(known, "{flag} has no test. Add it to a table in tests/flags.rs");
    }
}

#[test]
fn every_prompt_flag_has_a_test() {
    assert_all_classified(&["--help"]);
}

#[test]
fn every_chat_flag_has_a_test() {
    assert_all_classified(&["chat", "--help"]);
}

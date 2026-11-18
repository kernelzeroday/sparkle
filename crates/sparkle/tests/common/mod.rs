//! Shared test support: a mock OpenAI-compatible server, an isolated sparkle home, and a driver
//! that runs the real full-screen interface inside a pseudo-terminal.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};

pub const REPLY: &str = "Hello world\n\n```python\nprint('hi')\n```\n\n```sh\nls\n```";
pub const GENERATED_TITLE: &str = "Mock chat title";
const WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct Recorded {
    pub body: Value,
    pub authorization: Option<String>,
}

impl Recorded {
    fn is_title_request(&self) -> bool {
        self.body["messages"][0]["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("Write a short title"))
    }
}

pub struct Harness {
    pub home: PathBuf,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
}

fn sse(delta: Value) -> String {
    let chunk = json!({ "choices": [{ "index": 0, "delta": delta, "finish_reason": null }] });
    format!("data: {chunk}\n\n")
}

fn http_json(stream: &mut std::net::TcpStream, status: &str, payload: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
}

fn respond(stream: &mut std::net::TcpStream, recorded: &Recorded) {
    let body = &recorded.body;
    let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or_default();
    let text = last["content"].as_str().unwrap_or("");
    if text.trim_end().ends_with("http500") {
        http_json(stream, "500 Internal Server Error", r#"{"error":{"message":"mock exploded"}}"#);
        return;
    }
    if body["stream"] == json!(false) {
        let payload = json!({
            "choices": [{ "message": { "role": "assistant", "content": "whole reply" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 2, "completion_tokens": 3 }
        });
        http_json(stream, "200 OK", &payload.to_string());
        return;
    }
    let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
    if recorded.is_title_request() {
        let _ = write!(stream, "{}data: [DONE]\n\n", sse(json!({ "content": GENERATED_TITLE })));
        return;
    }
    if text.contains("midstream") {
        let _ = write!(stream, "{}data: {{\"error\":{{\"message\":\"stream broke\"}}}}\n\n", sse(json!({ "content": "partial " })));
        return;
    }
    let pause = Duration::from_millis(if text.contains("slowly") { 700 } else { 0 });
    let parts = [
        sse(json!({ "reasoning_content": "Thinking hard" })),
        sse(json!({ "content": "Hello " })),
        sse(json!({ "content": "world\n\n```python\nprint('hi')\n```\n\n```sh\nls\n```" })),
    ];
    for part in parts {
        if stream.write_all(part.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
        std::thread::sleep(pause);
    }
    let usage = json!({ "choices": [], "usage": { "prompt_tokens": 11, "completion_tokens": 7 } });
    let _ = write!(stream, "data: {usage}\n\ndata: [DONE]\n\n");
}

fn start_server(requests: Arc<Mutex<Vec<Recorded>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                let mut authorization = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.trim().split_once(':') {
                        match name.to_lowercase().as_str() {
                            "content-length" => length = value.trim().parse().unwrap_or(0),
                            "authorization" => authorization = Some(value.trim().to_string()),
                            _ => {}
                        }
                    }
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                let recorded = Recorded { body, authorization };
                requests.lock().unwrap().push(recorded.clone());
                respond(&mut stream, &recorded);
            });
        }
    });
    port
}

impl Harness {
    pub fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("sparkle-test-{}-{unique}-{:?}", std::process::id(), std::thread::current().id());
        let name: String = name.chars().filter(|c| c.is_alphanumeric() || *c == '-').collect();
        let home = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let port = start_server(Arc::clone(&requests));
        std::fs::write(
            home.join("extra-openai-models.yaml"),
            format!(
                "- model_id: mock\n  model_name: mock-wire\n  aliases: [mk]\n  api_base: http://127.0.0.1:{port}/v1\n  api_key_name: mockkey\n\
                 - model_id: mock2\n  model_name: mock2-wire\n  aliases: [mk2]\n  api_base: http://127.0.0.1:{port}/v1\n  api_key_name: mockkey\n"
            ),
        )
        .unwrap();
        std::fs::write(home.join("keys.json"), r#"{"mockkey":"sk-test"}"#).unwrap();
        std::fs::write(home.join("default_model.txt"), "mock\n").unwrap();
        Self { home, requests }
    }

    pub fn write_file(&self, name: &str, contents: &str) -> String {
        let path = self.home.join(name);
        std::fs::write(&path, contents).unwrap();
        path.to_str().unwrap().to_string()
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sparkle"));
        command
            .args(args)
            .env("SPARKLE_HOME", &self.home)
            .env("LLM_USER_PATH", &self.home)
            .stdin(Stdio::null());
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    pub fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        self.run_with_bytes(args, input.as_bytes())
    }

    pub fn run_with_bytes(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    /// Chat requests in order, leaving out background title requests.
    pub fn chat_requests(&self) -> Vec<Recorded> {
        let requests = self.requests.lock().unwrap();
        requests.iter().filter(|request| !request.is_title_request()).cloned().collect()
    }

    pub fn title_requests(&self) -> usize {
        self.requests.lock().unwrap().iter().filter(|request| request.is_title_request()).count()
    }

    pub fn last_request(&self) -> Recorded {
        self.chat_requests().last().expect("a chat request was made").clone()
    }

    pub fn last_body(&self) -> Value {
        self.last_request().body
    }

    pub fn wait_for_requests(&self, count: usize) {
        let start = Instant::now();
        while self.chat_requests().len() < count {
            assert!(start.elapsed() < WAIT, "expected {count} chat requests");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub const CTRL_C: &str = "\x03";
pub const CTRL_D: &str = "\x04";
pub const CTRL_T: &str = "\x14";
pub const CTRL_J: &str = "\n";
pub const ESC: &str = "\x1b";
pub const TAB: &str = "\t";
pub const SHIFT_TAB: &str = "\x1b[Z";
pub const READY: &str = "Message, or / for commands";

/// The real interface running in a pseudo-terminal.
pub struct Tui {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    raw: Arc<Mutex<Vec<u8>>>,
    child: Box<dyn Child + Send + Sync>,
}

impl Tui {
    /// Starts sparkle and waits for the chat to be ready.
    pub fn open(harness: &Harness, args: &[&str]) -> Self {
        let tui = Self::spawn(harness, args, &[]);
        tui.wait_for(READY);
        tui
    }

    pub fn spawn(harness: &Harness, args: &[&str], env: &[(&str, &str)]) -> Self {
        let (rows, cols) = (32, 100);
        let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
        let pair = native_pty_system().openpty(size).unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_sparkle"));
        command.args(args);
        command.cwd(&harness.home);
        command.env("SPARKLE_HOME", &harness.home);
        command.env("LLM_USER_PATH", &harness.home);
        command.env("TERM", "xterm-256color");
        command.env_remove("TERM_PROGRAM");
        command.env_remove("VISUAL");
        command.env_remove("EDITOR");
        for (name, value) in env {
            command.env(name, value);
        }
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);

        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (reply_to, screen, log) = (Arc::clone(&writer), Arc::clone(&parser), Arc::clone(&raw));
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let chunk = &buffer[..count];
                screen.lock().unwrap().process(chunk);
                log.lock().unwrap().extend_from_slice(chunk);
                // Answer the terminal queries a real terminal would answer.
                let contains = |needle: &[u8]| chunk.windows(needle.len()).any(|w| w == needle);
                if contains(b"\x1b[c") {
                    let _ = reply_to.lock().unwrap().write_all(b"\x1b[?62;c");
                }
                if contains(b"\x1b[6n") {
                    let _ = reply_to.lock().unwrap().write_all(b"\x1b[1;1R");
                }
            }
        });
        Self { master: pair.master, writer, parser, raw, child }
    }

    pub fn send(&self, keys: &str) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(keys.as_bytes()).unwrap();
        writer.flush().unwrap();
    }

    pub fn screen(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    pub fn raw(&self) -> String {
        String::from_utf8_lossy(&self.raw.lock().unwrap()).into_owned()
    }

    pub fn wait_for(&self, text: &str) {
        let start = Instant::now();
        while !self.screen().contains(text) {
            assert!(start.elapsed() < WAIT, "'{text}' never appeared on:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn wait_gone(&self, text: &str) {
        let start = Instant::now();
        while self.screen().contains(text) {
            assert!(start.elapsed() < WAIT, "'{text}' never left:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Sends a message and waits until the reply has finished.
    pub fn ask(&self, harness: &Harness, text: &str) {
        let before = harness.chat_requests().len();
        self.send(text);
        self.send("\r");
        let start = Instant::now();
        while harness.chat_requests().len() < before + 1 {
            assert!(start.elapsed() < WAIT, "'{text}' was never sent. Screen:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(200));
        self.wait_gone("esc to stop");
    }

    /// Runs a slash command and gives the interface a moment to apply it.
    pub fn command(&self, text: &str) {
        self.send(text);
        self.send("\r");
        self.wait_gone(text);
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).unwrap();
        self.parser.lock().unwrap().screen_mut().set_size(rows, cols);
    }

    /// Window titles the app set, oldest first.
    pub fn window_titles(&self) -> Vec<String> {
        self.raw()
            .split("\x1b]0;")
            .skip(1)
            .filter_map(|part| part.split('\x07').next().map(str::to_string))
            .collect()
    }

    pub fn wait_for_window_title(&self, title: &str) {
        let start = Instant::now();
        while self.window_titles().last().map(String::as_str) != Some(title) {
            assert!(start.elapsed() < WAIT, "title never became '{title}': {:?}", self.window_titles());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Waits for the program to end and returns whether it exited successfully.
    pub fn wait_exit(&mut self) -> bool {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Let the reader collect the final output.
                std::thread::sleep(Duration::from_millis(100));
                return status.success();
            }
            assert!(start.elapsed() < WAIT, "still running:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Clears a draft from the input, then leaves the chat.
    pub fn quit_after_clearing(&mut self) {
        self.send(CTRL_C);
        self.wait_for(READY);
        self.quit();
    }

    /// Leaves the chat with a double ctrl+c.
    pub fn quit(&mut self) {
        self.send(CTRL_C);
        self.wait_for("again to exit");
        self.send(CTRL_C);
        assert!(self.wait_exit(), "sparkle should exit cleanly");
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

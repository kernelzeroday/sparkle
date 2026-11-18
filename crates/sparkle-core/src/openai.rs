//! OpenAI-compatible `/chat/completions` provider (OpenAI, llama.cpp, Ollama, vLLM and others).

use std::collections::VecDeque;
use std::net::IpAddr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::future::BoxFuture;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

use crate::sse::SseDecoder;
use crate::{
    Attachment, ChatRequest, Error, EventStream, Message, PromptProgress, Provider, Result, Role,
    StreamEvent, Usage,
};

/// How long a local server gets to report its context size.
const PROPS_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct OpenAiCompat {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiCompat {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }

    fn host(&self) -> Option<String> {
        reqwest::Url::parse(&self.base_url).ok()?.host_str().map(str::to_string)
    }

    fn is_openai(&self) -> bool {
        self.host().is_some_and(|host| host == "openai.com" || host.ends_with(".openai.com"))
    }

    /// The body to send. Streams elsewhere ask for prefill progress, which llama-server reports.
    fn wire_body(&self, request: &ChatRequest) -> Value {
        let mut body = request_body(request);
        if request.stream
            && !self.is_openai()
            && let Some(map) = body.as_object_mut()
        {
            // An explicit `-o return_progress false` turns this off.
            map.entry("return_progress").or_insert(json!(true));
        }
        body
    }

    /// Where llama-server publishes its settings, which sit beside the `/v1` API.
    fn props_url(&self) -> String {
        format!("{}/props", self.base_url.strip_suffix("/v1").unwrap_or(&self.base_url))
    }

    async fn fetch_context_window(&self) -> Option<u64> {
        if !self.host().is_some_and(|host| is_local_host(&host)) {
            return None;
        }
        let mut builder = self.client.get(self.props_url()).timeout(PROPS_TIMEOUT);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder.send().await.ok()?.error_for_status().ok()?;
        parse_context_window(&response.text().await.ok()?)
    }
}

/// True for hosts on this machine or the local network.
fn is_local_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(address) = host.parse::<IpAddr>() {
        return match address.to_canonical() {
            IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
            IpAddr::V6(ip) => {
                let first = ip.segments()[0];
                ip.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
            }
        };
    }
    let host = host.to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local")
}

/// Reads the context size from a llama-server `/props` response.
fn parse_context_window(data: &str) -> Option<u64> {
    let value: Value = serde_json::from_str(data).ok()?;
    value["default_generation_settings"]["n_ctx"]
        .as_u64()
        .or_else(|| value["n_ctx"].as_u64())
        .filter(|size| *size > 0)
}

/// Encodes an attachment the way the chat completions API expects for its type.
fn attachment_part(attachment: &Attachment) -> Value {
    if let Attachment::Inline { mime, base64 } = attachment {
        if mime == "application/pdf" {
            return json!({
                "type": "file",
                "file": { "filename": "attachment.pdf", "file_data": attachment.as_url() },
            });
        }
        if mime.starts_with("audio/") {
            let format = if mime == "audio/wav" { "wav" } else { "mp3" };
            return json!({ "type": "input_audio", "input_audio": { "data": base64, "format": format } });
        }
        if mime.starts_with("text/") {
            let decoded = BASE64.decode(base64).unwrap_or_default();
            return json!({ "type": "text", "text": String::from_utf8_lossy(&decoded) });
        }
    }
    json!({ "type": "image_url", "image_url": { "url": attachment.as_url() } })
}

fn message_json(message: &Message) -> Value {
    if message.attachments.is_empty() || message.role != Role::User {
        return json!({ "role": message.role.as_str(), "content": message.content });
    }
    let mut parts = vec![json!({ "type": "text", "text": message.content })];
    parts.extend(message.attachments.iter().map(attachment_part));
    json!({ "role": message.role.as_str(), "content": parts })
}

/// Builds the request body. Explicit `-o` values win over friendly flags.
pub fn request_body(request: &ChatRequest) -> Value {
    let mut body = json!({
        "model": request.model,
        "stream": request.stream,
        "messages": request.messages.iter().map(message_json).collect::<Vec<_>>(),
    });
    let map = body.as_object_mut().expect("body is an object");
    if request.stream {
        map.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    let options = &request.options;
    if let Some(value) = &options.effort {
        map.insert("reasoning_effort".into(), json!(value));
    }
    if let Some(value) = options.temperature {
        map.insert("temperature".into(), json!(value));
    }
    if let Some(value) = options.top_p {
        map.insert("top_p".into(), json!(value));
    }
    if let Some(value) = options.max_tokens {
        map.insert("max_tokens".into(), json!(value));
    }
    if let Some(value) = options.seed {
        map.insert("seed".into(), json!(value));
    }
    if let Some(schema) = &request.schema {
        map.insert(
            "response_format".into(),
            json!({ "type": "json_schema", "json_schema": { "name": "output", "schema": schema } }),
        );
    }
    for (key, value) in &options.extra {
        map.insert(key.clone(), value.clone());
    }
    body
}

fn usage_event(value: &Value) -> Option<StreamEvent> {
    let usage = value.get("usage").filter(|usage| usage.is_object())?;
    Some(StreamEvent::Usage(Usage {
        input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
        output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
    }))
}

fn prompt_progress_event(value: &Value) -> Option<StreamEvent> {
    let progress = value.get("prompt_progress").filter(|progress| progress.is_object())?;
    let field = |name: &str| progress[name].as_u64().unwrap_or(0);
    let total = field("total");
    (total > 0).then(|| {
        StreamEvent::PromptProgress(PromptProgress {
            processed: field("processed"),
            total,
            cache: field("cache"),
            time_ms: field("time_ms"),
        })
    })
}

fn error_message(value: &Value) -> Option<String> {
    let error = value.get("error").filter(|error| !error.is_null())?;
    Some(error["message"].as_str().map_or_else(|| error.to_string(), str::to_string))
}

fn text_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| value[*name].as_str()).filter(|text| !text.is_empty())
}

/// Parses one streaming chunk. Reasoning arrives as `reasoning_content` or `reasoning`.
pub fn parse_chunk(data: &str) -> Result<Vec<StreamEvent>> {
    let value: Value = serde_json::from_str(data)?;
    if let Some(message) = error_message(&value) {
        return Err(Error::Stream(message));
    }
    let mut events: Vec<StreamEvent> = prompt_progress_event(&value).into_iter().collect();
    if let Some(choice) = value["choices"].get(0) {
        let delta = &choice["delta"];
        if let Some(text) = text_field(delta, &["reasoning_content", "reasoning"]) {
            events.push(StreamEvent::Reasoning(text.to_string()));
        }
        if let Some(text) = text_field(delta, &["content"]) {
            events.push(StreamEvent::Text(text.to_string()));
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            events.push(StreamEvent::Finished(reason.to_string()));
        }
    }
    events.extend(usage_event(&value));
    Ok(events)
}

/// Parses a complete non-streaming response into the same events a stream would produce.
pub fn parse_response(data: &str) -> Result<Vec<StreamEvent>> {
    let value: Value = serde_json::from_str(data)?;
    if let Some(message) = error_message(&value) {
        return Err(Error::Stream(message));
    }
    let mut events = Vec::new();
    if let Some(choice) = value["choices"].get(0) {
        let message = &choice["message"];
        if let Some(text) = text_field(message, &["reasoning_content", "reasoning"]) {
            events.push(StreamEvent::Reasoning(text.to_string()));
        }
        if let Some(text) = text_field(message, &["content"]) {
            events.push(StreamEvent::Text(text.to_string()));
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            events.push(StreamEvent::Finished(reason.to_string()));
        }
    }
    events.extend(usage_event(&value));
    Ok(events)
}

fn sse_events(response: reqwest::Response) -> EventStream {
    struct State<S> {
        bytes: S,
        decoder: SseDecoder,
        queue: VecDeque<Result<StreamEvent>>,
        done: bool,
    }

    impl<S> State<S> {
        /// Queues the events in one payload and reports whether the stream has ended.
        fn accept(&mut self, data: &str) {
            if data.trim() == "[DONE]" {
                self.done = true;
                return;
            }
            match parse_chunk(data) {
                Ok(events) => self.queue.extend(events.into_iter().map(Ok)),
                Err(error) => {
                    self.queue.push_back(Err(error));
                    self.done = true;
                }
            }
        }
    }

    let state = State {
        bytes: Box::pin(response.bytes_stream()),
        decoder: SseDecoder::default(),
        queue: VecDeque::new(),
        done: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.queue.pop_front() {
                return Some((event, state));
            }
            if state.done {
                return None;
            }
            match state.bytes.next().await {
                Some(Ok(chunk)) => {
                    for data in state.decoder.push(&chunk) {
                        state.accept(&data);
                        if state.done {
                            break;
                        }
                    }
                }
                Some(Err(error)) => {
                    state.queue.push_back(Err(error.into()));
                    state.done = true;
                }
                None => {
                    if let Some(data) = state.decoder.finish() {
                        state.accept(&data);
                    }
                    state.done = true;
                }
            }
        }
    }))
}

impl Provider for OpenAiCompat {
    fn stream<'a>(&'a self, request: &'a ChatRequest) -> BoxFuture<'a, Result<EventStream>> {
        Box::pin(async move {
            let url = format!("{}/chat/completions", self.base_url);
            let mut builder = self
                .client
                .post(url)
                .header("content-type", "application/json")
                .body(self.wire_body(request).to_string());
            if let Some(key) = &self.api_key {
                builder = builder.bearer_auth(key);
            }
            let response = builder.send().await?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                let message = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|value| error_message(&value))
                    .unwrap_or(text);
                return Err(Error::Api { status: status.as_u16(), message });
            }
            if request.stream {
                return Ok(sse_events(response));
            }
            let events = parse_response(&response.text().await?)?;
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))) as EventStream)
        })
    }

    fn context_window(&self) -> BoxFuture<'_, Option<u64>> {
        Box::pin(self.fetch_context_window())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Options;

    fn request() -> ChatRequest {
        ChatRequest {
            model: "mock".into(),
            messages: vec![Message::new(Role::System, "SYS"), Message::new(Role::User, "x")],
            options: Options::default(),
            schema: None,
            stream: true,
        }
    }

    #[test]
    fn body_matches_the_shape_python_llm_sends() {
        let mut request = request();
        request.options.effort = Some("low".into());
        request.options.temperature = Some(0.3);
        assert_eq!(
            request_body(&request),
            json!({
                "model": "mock",
                "stream": true,
                "messages": [
                    { "role": "system", "content": "SYS" },
                    { "role": "user", "content": "x" }
                ],
                "reasoning_effort": "low",
                "stream_options": { "include_usage": true },
                "temperature": 0.3
            })
        );
    }

    #[test]
    fn explicit_option_overrides_friendly_flag() {
        let mut request = request();
        request.options.effort = Some("low".into());
        request.options.extra.insert("reasoning_effort".into(), json!("high"));
        assert_eq!(request_body(&request)["reasoning_effort"], "high");
    }

    #[test]
    fn attachments_become_image_parts() {
        let mut request = request();
        request.messages[1].attachments.push(Attachment::Inline {
            mime: "image/png".into(),
            base64: "AAAA".into(),
        });
        assert_eq!(
            request_body(&request)["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn each_attachment_type_uses_its_own_part() {
        let inline = |mime: &str, base64: &str| Attachment::Inline {
            mime: mime.into(),
            base64: base64.into(),
        };
        let pdf = attachment_part(&inline("application/pdf", "AAAA"));
        assert_eq!(pdf["type"], "file");
        assert_eq!(pdf["file"]["file_data"], "data:application/pdf;base64,AAAA");
        let wav = attachment_part(&inline("audio/wav", "AAAA"));
        assert_eq!(wav["input_audio"], json!({ "data": "AAAA", "format": "wav" }));
        let mp3 = attachment_part(&inline("audio/mpeg", "AAAA"));
        assert_eq!(mp3["input_audio"]["format"], "mp3");
        let text = attachment_part(&inline("text/plain", "aGk="));
        assert_eq!(text, json!({ "type": "text", "text": "hi" }));
        let url = attachment_part(&Attachment::Url("https://a.io/x.png".into()));
        assert_eq!(url["image_url"]["url"], "https://a.io/x.png");
    }

    #[test]
    fn parses_reasoning_text_finish_and_usage() {
        let reasoning = r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#;
        assert_eq!(parse_chunk(reasoning).unwrap(), vec![StreamEvent::Reasoning("hm".into())]);
        let alternate = r#"{"choices":[{"delta":{"reasoning":"hm"}}]}"#;
        assert_eq!(parse_chunk(alternate).unwrap(), vec![StreamEvent::Reasoning("hm".into())]);
        let text = r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;
        assert_eq!(
            parse_chunk(text).unwrap(),
            vec![StreamEvent::Text("hi".into()), StreamEvent::Finished("stop".into())]
        );
        let usage = r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#;
        assert_eq!(
            parse_chunk(usage).unwrap(),
            vec![StreamEvent::Usage(Usage { input_tokens: 11, output_tokens: 7 })]
        );
    }

    #[test]
    fn parses_prompt_progress() {
        let chunk = r#"{"choices":[],"prompt_progress":{"total":53,"cache":4,"processed":20,"time_ms":50}}"#;
        let progress = PromptProgress { processed: 20, total: 53, cache: 4, time_ms: 50 };
        assert_eq!(parse_chunk(chunk).unwrap(), vec![StreamEvent::PromptProgress(progress)]);
    }

    #[test]
    fn prompt_progress_comes_before_text_in_the_same_chunk() {
        let chunk = r#"{"choices":[{"delta":{"content":"hi"}}],"prompt_progress":{"total":2,"processed":2}}"#;
        let events = parse_chunk(chunk).unwrap();
        assert!(matches!(events[0], StreamEvent::PromptProgress(_)));
        assert_eq!(events[1], StreamEvent::Text("hi".into()));
    }

    #[test]
    fn unusable_prompt_progress_is_ignored() {
        for chunk in [
            r#"{"choices":[],"prompt_progress":null}"#,
            r#"{"choices":[],"prompt_progress":"soon"}"#,
            r#"{"choices":[],"prompt_progress":{"total":0,"processed":0}}"#,
        ] {
            assert_eq!(parse_chunk(chunk).unwrap(), vec![], "{chunk}");
        }
    }

    #[test]
    fn progress_labels_show_percent_and_cache() {
        let progress = PromptProgress { processed: 83_000, total: 197_000, cache: 0, time_ms: 0 };
        assert_eq!(progress.percent(), 42);
        assert_eq!(progress.label(), "Prefilling 42% (83k/197k)");
        let cached = PromptProgress { cache: 12_000, ..progress };
        assert_eq!(cached.label(), "Prefilling 42% (83k/197k), 12k cached");
    }

    #[test]
    fn progress_percent_stays_in_range() {
        let almost = PromptProgress { processed: 999, total: 1000, ..Default::default() };
        assert_eq!(almost.percent(), 99);
        let over = PromptProgress { processed: 12, total: 10, ..Default::default() };
        assert_eq!(over.percent(), 100);
        assert_eq!(PromptProgress::default().percent(), 0);
        let huge = PromptProgress { processed: u64::MAX, total: u64::MAX, ..Default::default() };
        assert_eq!(huge.percent(), 100);
    }

    #[test]
    fn only_streams_to_other_servers_ask_for_progress() {
        let local = OpenAiCompat::new("http://127.0.0.1:8080/v1", None);
        assert_eq!(local.wire_body(&request())["return_progress"], true);

        let mut whole = request();
        whole.stream = false;
        assert!(local.wire_body(&whole).get("return_progress").is_none());

        for url in ["https://api.openai.com/v1", "https://API.OpenAI.com/v1/"] {
            let openai = OpenAiCompat::new(url, None);
            assert!(openai.wire_body(&request()).get("return_progress").is_none(), "{url}");
        }
        let lookalike = OpenAiCompat::new("https://api.openai.com.example.org/v1", None);
        assert_eq!(lookalike.wire_body(&request())["return_progress"], true);
    }

    #[test]
    fn an_explicit_option_can_turn_progress_off() {
        let mut request = request();
        request.options.extra.insert("return_progress".into(), json!(false));
        let local = OpenAiCompat::new("http://localhost:8080/v1", None);
        assert_eq!(local.wire_body(&request)["return_progress"], false);
    }

    #[test]
    fn props_sit_beside_the_versioned_api() {
        let versioned = OpenAiCompat::new("http://localhost:8080/v1/", None);
        assert_eq!(versioned.props_url(), "http://localhost:8080/props");
        let bare = OpenAiCompat::new("http://localhost:8080", None);
        assert_eq!(bare.props_url(), "http://localhost:8080/props");
    }

    #[test]
    fn reads_the_context_size_from_props() {
        let nested = r#"{"default_generation_settings":{"n_ctx":258000}}"#;
        assert_eq!(parse_context_window(nested), Some(258_000));
        assert_eq!(parse_context_window(r#"{"n_ctx":4096}"#), Some(4096));
        assert_eq!(parse_context_window(r#"{"n_ctx":0}"#), None);
        assert_eq!(parse_context_window("<html>not json</html>"), None);
    }

    #[test]
    fn recognizes_local_hosts() {
        for host in [
            "localhost", "LOCALHOST", "box.local", "app.localhost", "127.0.0.1", "10.0.0.8",
            "172.16.4.2", "192.168.1.20", "169.254.0.9", "[::1]", "[fd00::1]", "[fe80::1]",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(is_local_host(host), "{host}");
        }
        for host in ["api.openai.com", "8.8.8.8", "172.32.0.1", "[2001:db8::1]", "localhost.evil.io"] {
            assert!(!is_local_host(host), "{host}");
        }
    }

    #[tokio::test]
    async fn remote_servers_are_never_asked_for_props() {
        let remote = OpenAiCompat::new("https://api.example.invalid/v1", Some("secret".into()));
        assert_eq!(remote.context_window().await, None);
    }

    #[test]
    fn mid_stream_error_is_reported() {
        let error = parse_chunk(r#"{"error":{"message":"boom"}}"#).unwrap_err();
        assert_eq!(error.to_string(), "the server sent an error: boom");
    }
}

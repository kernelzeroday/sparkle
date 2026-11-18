//! OpenAI-compatible `/chat/completions` provider (OpenAI, llama.cpp, Ollama, vLLM and others).

use std::collections::VecDeque;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::future::BoxFuture;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

use crate::sse::SseDecoder;
use crate::{
    Attachment, ChatRequest, Error, EventStream, Message, Provider, Result, Role, StreamEvent, Usage,
};

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
    let mut events = Vec::new();
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
                .body(request_body(request).to_string());
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
    fn mid_stream_error_is_reported() {
        let error = parse_chunk(r#"{"error":{"message":"boom"}}"#).unwrap_err();
        assert_eq!(error.to_string(), "the server sent an error: boom");
    }
}

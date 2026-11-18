use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::context::compact_count;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Role::System),
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            _ => None,
        }
    }
}

/// Identifies common attachment types from their first bytes, for stdin and extensionless files.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    let riff = |kind: &[u8]| bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(kind);
    match bytes {
        _ if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => Some("image/png"),
        _ if bytes.starts_with(&[0xff, 0xd8, 0xff]) => Some("image/jpeg"),
        _ if bytes.starts_with(b"GIF8") => Some("image/gif"),
        _ if riff(b"WEBP") => Some("image/webp"),
        _ if riff(b"WAVE") => Some("audio/wav"),
        _ if bytes.starts_with(b"%PDF") => Some("application/pdf"),
        _ if bytes.starts_with(b"ID3") || bytes.starts_with(&[0xff, 0xfb]) => Some("audio/mpeg"),
        _ => None,
    }
}

/// Something sent alongside a message: a remote URL or inline base64 data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attachment {
    Url(String),
    Inline { mime: String, base64: String },
}

impl Attachment {
    /// Loads a URL, a file path, or raw bytes already read from stdin (`piped`).
    pub fn load(source: &str, mime: Option<&str>, piped: Option<&[u8]>) -> crate::Result<Self> {
        use base64::Engine;
        if source.starts_with("http://") || source.starts_with("https://") {
            return Ok(Attachment::Url(source.to_string()));
        }
        let bytes = match piped {
            Some(bytes) => bytes.to_vec(),
            None => std::fs::read(source).map_err(|error| {
                crate::Error::Config(format!("could not read attachment {source}: {error}"))
            })?,
        };
        let mime = match mime {
            Some(mime) => mime.to_string(),
            None => mime_guess::from_path(source)
                .first()
                .map(|mime| mime.to_string())
                .or_else(|| sniff_mime(&bytes).map(str::to_string))
                .unwrap_or_else(|| "application/octet-stream".to_string()),
        };
        Ok(Attachment::Inline {
            mime,
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    pub fn as_url(&self) -> String {
        match self {
            Attachment::Url(url) => url.clone(),
            Attachment::Inline { mime, base64 } => format!("data:{mime};base64,{base64}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub attachments: Vec<Attachment>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self { role, content: content.into(), attachments: Vec::new() }
    }
}

/// Model options. Friendly fields map to wire names, and `extra` carries `-o key value` pairs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Options {
    pub effort: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u64>,
    pub seed: Option<i64>,
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub options: Options,
    pub schema: Option<Value>,
    pub stream: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// How far a local server has read into the prompt before its first token.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptProgress {
    /// Tokens read so far, including those reused from the cache.
    pub processed: u64,
    pub total: u64,
    /// Tokens reused from the server's prompt cache.
    pub cache: u64,
    pub time_ms: u64,
}

impl PromptProgress {
    /// Whole percent complete, which only reaches 100 once every token is read.
    pub fn percent(&self) -> u64 {
        let processed = u128::from(self.processed.min(self.total));
        (processed * 100 / u128::from(self.total.max(1))) as u64
    }

    /// Status text such as `Prefilling 42% (83k/197k), 12k cached`.
    pub fn label(&self) -> String {
        let processed = compact_count(self.processed.min(self.total));
        let total = compact_count(self.total);
        let mut label = format!("Prefilling {}% ({processed}/{total})", self.percent());
        if self.cache > 0 {
            label.push_str(&format!(", {} cached", compact_count(self.cache)));
        }
        label
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    PromptProgress(PromptProgress),
    Reasoning(String),
    Text(String),
    Usage(Usage),
    Finished(String),
}

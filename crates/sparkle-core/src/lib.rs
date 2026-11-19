//! Domain types, configuration and model providers shared by every sparkle front end.

pub mod compact;
pub mod config;
pub mod context;
pub mod error;
pub mod extract;
pub mod journal;
pub mod library;
pub mod openai;
pub mod personas;
pub mod prefill;
pub mod provider;
pub mod schema;
pub mod setting;
pub mod sse;
pub mod title;
pub mod types;

pub use error::{Error, Result};
pub use provider::{EventStream, Provider};
pub use types::{
    Attachment, ChatRequest, Message, Options, PromptProgress, Role, StreamEvent, Usage,
};

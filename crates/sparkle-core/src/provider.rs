use std::pin::Pin;

use futures_util::Stream;
use futures_util::future::BoxFuture;

use crate::{ChatRequest, Result, StreamEvent};

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

/// A model backend. Non-streaming requests are delivered through the same event stream.
pub trait Provider: Send + Sync {
    fn stream<'a>(&'a self, request: &'a ChatRequest) -> BoxFuture<'a, Result<EventStream>>;

    /// The model's context size in tokens, for backends that can report it.
    fn context_window(&self) -> BoxFuture<'_, Option<u64>> {
        Box::pin(async { None })
    }
}

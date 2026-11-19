//! Streaming requests, event handling, cancellation and persistence.

use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use sparkle_core::prefill::PrefillFilter;
use sparkle_core::{ChatRequest, Role, StreamEvent};
use sparkle_store::NewMessage;

use super::{App, Payload, Phase, StreamMessage, Turn};

impl App {
    pub(super) fn begin_reply(&mut self) {
        let prefill = self.prefill.take();
        self.reply_filter = PrefillFilter::new(prefill.as_deref().unwrap_or(""));
        let request = ChatRequest {
            model: self.wire_model.clone(),
            messages: self.request_messages(prefill.as_deref()),
            options: self.options.clone(),
            schema: None,
            stream: true,
        };
        let mut turn = Turn::new(Role::Assistant, prefill.unwrap_or_default());
        turn.model.clone_from(&self.model);
        turn.parent_id = self.tip_id();
        self.turns.push(turn);
        self.phase = Phase::Waiting;
        self.start_stream(request);
    }

    /// Sends a request in the background, with its events arriving through `on_stream`.
    pub(super) fn start_stream(&mut self, request: ChatRequest) {
        self.started = Some(Instant::now());
        self.first_reasoning = None;
        self.prompt_progress = None;
        self.generation += 1;

        let generation = self.generation;
        let sender = self.sender.clone();
        let provider = Arc::clone(&self.provider);
        self.task = Some(tokio::spawn(async move {
            let result = async {
                let mut stream = provider.stream(&request).await?;
                while let Some(event) = stream.next().await {
                    let payload = Payload::Event(event?);
                    if sender.send(StreamMessage { generation, payload }).is_err() {
                        break;
                    }
                }
                Ok::<(), sparkle_core::Error>(())
            }
            .await;
            let payload = match result {
                Ok(()) => Payload::Ended,
                Err(error) => Payload::Failed(error.to_string()),
            };
            let _ = sender.send(StreamMessage { generation, payload });
        }));
    }

    pub fn on_stream(&mut self, message: StreamMessage) {
        if let Payload::Title(title) = &message.payload {
            if message.generation == self.title_epoch {
                self.apply_generated_title(title.clone());
            }
            return;
        }
        if let Payload::ContextWindow(size) = message.payload {
            if message.generation == self.context_epoch {
                self.context_window = Some(size);
                self.dirty = true;
            }
            return;
        }
        if message.generation != self.generation || !self.busy() {
            return;
        }
        self.dirty = true;
        if self.phase == Phase::Compacting {
            self.on_compaction(message.payload);
            return;
        }
        match message.payload {
            Payload::Event(event) => self.on_event(event),
            Payload::Failed(error) => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.error = Some(error);
                }
                self.finish();
            }
            Payload::Ended => self.finish(),
            Payload::Title(_) | Payload::ContextWindow(_) => {}
        }
    }

    fn elapsed_ms(since: Option<Instant>) -> Option<u64> {
        since.map(|start| start.elapsed().as_millis() as u64)
    }

    fn on_event(&mut self, event: StreamEvent) {
        // Progress changes nothing in the transcript, so its rendered lines stay cached.
        if let StreamEvent::PromptProgress(progress) = event {
            if self.phase == Phase::Waiting {
                self.prompt_progress = Some(progress);
            }
            return;
        }
        let thought = Self::elapsed_ms(self.first_reasoning);
        let counted = self.turns.len();
        let Some(turn) = self.turns.last_mut() else { return };
        turn.cache = None;
        match event {
            StreamEvent::PromptProgress(_) => {}
            StreamEvent::Reasoning(text) => {
                turn.reasoning.push_str(&text);
                self.first_reasoning.get_or_insert_with(Instant::now);
                self.phase = Phase::Thinking;
                self.prompt_progress = None;
            }
            StreamEvent::Text(text) => {
                turn.text.push_str(&self.reply_filter.push(&text));
                turn.thought_ms = turn.thought_ms.or(thought);
                self.phase = Phase::Writing;
                self.prompt_progress = None;
            }
            StreamEvent::Usage(usage) => {
                turn.usage = Some(usage);
                self.total_usage.input_tokens += usage.input_tokens;
                self.total_usage.output_tokens += usage.output_tokens;
                if usage.input_tokens > 0 {
                    let tokens = usage.input_tokens + usage.output_tokens;
                    self.measured_context = Some((tokens, counted));
                }
            }
            StreamEvent::Finished(_) => {}
        }
    }

    /// Stops the reply in progress and keeps what has arrived so far.
    pub fn cancel(&mut self) {
        if !self.busy() {
            return;
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if self.stop_compaction().is_some() {
            self.notice = Some("Compaction stopped. The conversation is unchanged".into());
            return;
        }
        if let Some(turn) = self.turns.last_mut() {
            turn.interrupted = true;
        }
        self.finish();
    }

    fn finish(&mut self) {
        let duration = Self::elapsed_ms(self.started);
        let thought = Self::elapsed_ms(self.first_reasoning);
        self.phase = Phase::Idle;
        self.started = None;
        self.prompt_progress = None;
        self.task = None;
        self.dirty = true;
        let conversation = self.conversation_id.clone();
        let Some(turn) = self.turns.last_mut() else { return };
        turn.text.push_str(&self.reply_filter.finish());
        turn.cache = None;
        turn.duration_ms = duration;
        if !turn.reasoning.is_empty() {
            turn.thought_ms = turn.thought_ms.or(thought);
        }
        // A reply stopped before any text arrived has nothing worth resending later.
        let saved = !turn.text.is_empty();
        if let (Some(store), Some(conversation), true) = (&self.store, conversation, saved) {
            let message = NewMessage {
                parent_id: turn.parent_id.as_deref(),
                role: Role::Assistant,
                content: &turn.text,
                reasoning: (!turn.reasoning.is_empty()).then_some(turn.reasoning.as_str()),
                attachments: &[],
                model: Some(&turn.model),
                usage: turn.usage,
                duration_ms: duration,
            };
            match store.append(&conversation, message) {
                Ok(id) => turn.stored_id = Some(id),
                Err(error) => self.notice = Some(format!("Could not save history: {error}")),
            }
        }
        self.request_title();
    }
}

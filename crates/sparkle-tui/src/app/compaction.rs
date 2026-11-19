//! Conversation compaction lifecycle and persistence.
//!
//! Compaction inserts a divider/summary entry inline after the last completed reply. Earlier turns
//! stay visible in the transcript scrollback above the divider, while the LLM only receives the
//! system prompt, the latest compaction summary, and turns that follow it.

use sparkle_core::compact::{self, Method};
use sparkle_core::context::{compact_count, estimate_tokens};
use sparkle_core::{Role, StreamEvent, Usage};
use sparkle_store::{CompactionInfo, NewMessage};

use super::{App, Compaction, Payload, Phase, Turn};

const DEFAULT_KEEP_RECENT: usize = 2;

impl App {
    /// Compacts active turns up to the last completed reply using the requested method (`summary`,
    /// `handoff`, `shake`, or `trim`). Anything after that reply, such as a message that failed to
    /// send, stays word for word after the divider.
    pub(super) fn begin_compaction(&mut self, argument: &str) {
        let (method, focus) = compact::parse_command(argument);
        let active_start = self.active_turns_start();
        let Some(offset) = self.turns[active_start..].iter().rposition(Turn::is_completed_reply)
        else {
            self.notice = Some("Nothing to compact yet. Wait for a finished reply first".into());
            return;
        };
        let replaced = active_start + offset + 1;

        if method.is_local() {
            let messages = self.wire_messages(&self.turns[..replaced]);
            let keep_recent = focus
                .and_then(|value| value.split_whitespace().next())
                .and_then(|word| word.parse::<usize>().ok())
                .unwrap_or(DEFAULT_KEEP_RECENT);
            let result = match method {
                Method::Shake => compact::shake(&messages, keep_recent)
                    .ok_or_else(|| "nothing large enough to elide".to_string()),
                Method::Trim => compact::trim(&messages, keep_recent)
                    .ok_or_else(|| "not enough earlier messages to drop".to_string()),
                _ => unreachable!(),
            };
            let applied =
                result.and_then(|text| self.commit_compaction(method, replaced, text, None));
            if let Err(reason) = applied {
                let text = format!("Compaction failed, so the conversation is unchanged: {reason}");
                self.turns.push(Turn::notice(text));
            }
            return;
        }

        // A local server may handle one request at a time, so the summary goes first.
        self.stop_title_request();
        let request = compact::request_with_method(
            method,
            &self.wire_model,
            self.wire_messages(&self.turns[..replaced]),
            self.options.clone(),
            focus,
        );
        self.compaction = Some(Compaction { replaced, method, ..Compaction::default() });
        self.phase = Phase::Compacting;
        self.start_stream(request);
    }

    pub(super) fn on_compaction(&mut self, payload: Payload) {
        let Some(compaction) = &mut self.compaction else { return };
        match payload {
            Payload::Event(StreamEvent::PromptProgress(progress)) => {
                if compaction.reply.is_empty() {
                    self.prompt_progress = Some(progress);
                }
            }
            Payload::Event(StreamEvent::Reasoning(_)) => self.prompt_progress = None,
            Payload::Event(StreamEvent::Text(text)) => {
                compaction.reply.push_str(&text);
                self.prompt_progress = None;
            }
            // The server counted the old conversation, so this says nothing about the new one.
            Payload::Event(StreamEvent::Usage(usage)) => {
                compaction.usage = Some(usage);
                self.total_usage.input_tokens += usage.input_tokens;
                self.total_usage.output_tokens += usage.output_tokens;
            }
            Payload::Event(StreamEvent::Finished(reason)) => {
                compaction.truncated = reason == "length";
            }
            Payload::Failed(error) => self.finish_compaction(Err(error)),
            Payload::Ended => self.finish_compaction(Ok(())),
            Payload::Title(_) | Payload::ContextWindow(_) => {}
        }
    }

    /// Returns to idle and hands back the compaction, if one was running.
    pub(super) fn stop_compaction(&mut self) -> Option<Compaction> {
        self.compaction.as_ref()?;
        self.phase = Phase::Idle;
        self.started = None;
        self.prompt_progress = None;
        self.task = None;
        self.dirty = true;
        self.compaction.take()
    }

    fn finish_compaction(&mut self, outcome: std::result::Result<(), String>) {
        let Some(compaction) = self.stop_compaction() else { return };
        let applied = outcome.and_then(|()| {
            if compaction.truncated {
                return Err("the summary was cut off by the token limit".into());
            }
            let text = compact::summary_from_reply(&compaction.reply)
                .ok_or("the model returned an empty summary")?;
            self.commit_compaction(compaction.method, compaction.replaced, text, compaction.usage)
        });
        if let Err(reason) = applied {
            let text = format!("Compaction failed, so the conversation is unchanged: {reason}");
            self.turns.push(Turn::notice(text));
        }
    }

    /// Inserts the compaction divider inline at `replaced`, leaving prior turns visible above it.
    fn commit_compaction(
        &mut self,
        method: Method,
        replaced: usize,
        text: String,
        usage: Option<Usage>,
    ) -> std::result::Result<(), String> {
        let active_start = self.active_turns_start();
        let sent = || self.turns[active_start..replaced].iter().filter(|turn| turn.is_sent());
        let (count, before_bytes) = (sent().count(), sent().map(Turn::wire_len).sum::<usize>());

        let probe = Turn::summary(text.clone(), None);
        let after_bytes = probe.wire_len();
        if after_bytes >= before_bytes {
            return Err("the summary was no shorter than the messages it would replace".into());
        }

        let tokens_before = estimate_tokens(before_bytes);
        let tokens_after = estimate_tokens(after_bytes);
        let info = CompactionInfo {
            method: method.as_str().to_string(),
            tokens_before,
            tokens_after,
        };
        let mut summary = Turn::summary(text, Some(info.clone()));
        summary.model.clone_from(&self.model);
        summary.usage = usage;
        summary.parent_id =
            self.turns[..replaced].iter().rev().find_map(|turn| turn.stored_id.clone());

        let saved = self
            .save_compaction(&summary, replaced, &info)
            .map_err(|error| format!("could not save the summary: {error}"))?;
        if let Some((summary_id, kept_ids)) = saved {
            summary.stored_id = Some(summary_id.clone());
            self.relink_kept_turns(replaced, summary_id, kept_ids);
        }

        self.turns.insert(replaced, summary);
        self.measured_context = None;
        self.dirty = true;
        self.scroll = 0;
        self.notice = Some(format!(
            "Compacted {count} message{} via {} (~{} → ~{} tokens) · ctrl+o to expand",
            if count == 1 { "" } else { "s" },
            method.as_str(),
            compact_count(tokens_before),
            compact_count(tokens_after),
        ));
        Ok(())
    }

    /// Stores the inline compaction entry and re-appends any turns kept after it, when this chat is
    /// being saved.
    fn save_compaction(
        &self,
        summary: &Turn,
        replaced: usize,
        info: &CompactionInfo,
    ) -> sparkle_store::Result<Option<(String, Vec<String>)>> {
        let (Some(store), Some(conversation), Some(compacted_from)) =
            (&self.store, &self.conversation_id, summary.parent_id.as_deref())
        else {
            return Ok(None);
        };
        let content = summary.wire_text();
        let message = NewMessage {
            model: Some(&summary.model),
            usage: summary.usage,
            ..NewMessage::plain(Some(compacted_from), Role::User, &content)
        };
        let summary_id = store.append_compaction(conversation, message, compacted_from, info)?;
        let mut kept_ids = Vec::new();
        let mut parent = summary_id.clone();
        for turn in self.turns[replaced..].iter().filter(|turn| turn.is_sent()) {
            let role = turn.role.expect("sent turns have a role");
            let wire = turn.wire_text();
            let copy = NewMessage {
                parent_id: Some(parent.as_str()),
                role,
                content: &wire,
                reasoning: (!turn.reasoning.is_empty()).then_some(turn.reasoning.as_str()),
                attachments: &turn.attachments,
                model: (!turn.model.is_empty()).then_some(turn.model.as_str()),
                usage: turn.usage,
                duration_ms: turn.duration_ms,
            };
            let id = store.append(conversation, copy)?;
            parent = id.clone();
            kept_ids.push(id);
        }
        Ok(Some((summary_id, kept_ids)))
    }

    /// Points the kept turns at their copies on the compacted branch so later edits stay linked.
    fn relink_kept_turns(&mut self, replaced: usize, summary_id: String, kept_ids: Vec<String>) {
        let mut parent = summary_id;
        let mut ids = kept_ids.into_iter();
        for turn in &mut self.turns[replaced..] {
            if turn.is_sent()
                && let Some(id) = ids.next()
            {
                turn.parent_id = Some(parent);
                turn.stored_id = Some(id.clone());
                parent = id;
            }
        }
    }
}

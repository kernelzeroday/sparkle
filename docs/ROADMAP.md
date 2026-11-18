# sparkle roadmap

This page records direction and scope for context handling. [PLAN.md](PLAN.md) holds the full
feature map and milestones.

## Status

| Item | Status | Target |
| --- | --- | --- |
| Prefill progress from `llama-server` | done | |
| System prompt, fragment and prefill sizes in the chat interface | done | |
| Context usage for local servers that report `n_ctx` through `/props` | done | |
| Manual compaction with `/compact` | done | |
| System prompt library, `/prompts` browser and context warnings | done | |
| Automatic compaction | planned, not implemented | M7 |
| Context size for remote providers | planned, not implemented | M7 |
| Memory tools | out of scope until after 1.0 | after 1.0 |

## Context compaction

- **`/compact [FOCUS]`** (done) summarizes every turn up to the last finished reply with the
  current model and continues the chat from that summary. The system prompt is left alone, and
  anything after the last finished reply, such as a message that failed to send, is kept word for
  word. Stopping the request, a server error, or a summary that is empty, cut off or no shorter
  leaves the conversation unchanged.
- **History stays intact** (done). In keeping with "never lose the user's words", the summary
  starts a new branch in the message tree and records the tip of the branch it replaced in
  `messages.compacted_from`. Stored messages are never rewritten or deleted, and `--cid` and `-c`
  resume from the summary.
- **Automatic compaction** (planned) will offer the same step as a conversation nears the context
  limit. It depends on a trustworthy context size, so it stays off wherever that size is unknown.

## Out of scope until after 1.0: memory tools

Basic memory tools, meaning tools that let a model read and write persistent memory across
conversations, are explicitly out of scope until after a solid 1.0 release. Proposals to add them
sooner should wait.

The **M3 Memory** milestone in the plan is a different thing. It covers the history browser,
search, branches and log import, all of which the user drives directly.

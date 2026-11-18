# sparkle roadmap

This page records direction and scope for context handling. [PLAN.md](PLAN.md) holds the full
feature map and milestones.

## Status

| Item | Status | Target |
| --- | --- | --- |
| Prefill progress from `llama-server` | done | |
| System prompt, fragment and prefill sizes in the chat interface | done | |
| Context usage for local servers that report `n_ctx` through `/props` | done | |
| Manual compaction with `/compact` | planned, not implemented | M7 |
| Automatic compaction | planned, not implemented | M7 |
| Context size for remote providers | planned, not implemented | M7 |
| Memory tools | out of scope until after 1.0 | after 1.0 |

## Planned: context compaction

Neither `/compact` nor automatic compaction exists yet. Today a conversation grows until the
server rejects it or the user starts a new one with `/new`.

- **`/compact`** will summarize earlier turns with the current model and continue the chat from
  that summary.
- **Automatic compaction** will offer the same step as a conversation nears the context limit. It
  depends on a trustworthy context size, so it stays off wherever that size is unknown.
- **History stays intact.** In keeping with "never lose the user's words", compaction will add a
  branch to the message tree and will not rewrite or delete stored messages.

## Out of scope until after 1.0: memory tools

Basic memory tools, meaning tools that let a model read and write persistent memory across
conversations, are explicitly out of scope until after a solid 1.0 release. Proposals to add them
sooner should wait.

The **M3 Memory** milestone in the plan is a different thing. It covers the history browser,
search, branches and log import, all of which the user drives directly.

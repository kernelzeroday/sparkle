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

- **`/compact [summary|handoff|shake|trim] [FOCUS]`** (done) compacts active turns up to the last
  finished reply and inserts an inline divider (`── ▸ compacted (<method>) · ~before → ~after · ctrl+o ──`)
  into the transcript. Earlier messages stay visible above the divider (including across resume),
  while the model receives only the system prompt, the latest compaction summary, and turns after
  it. Press `ctrl+o` to expand or collapse the summary. Supported methods:
  - `summary` (default): concise LLM summary, optionally guided by `[FOCUS]`.
  - `handoff`: structured LLM handoff document (Goal, Constraints, Progress, Decisions, Context).
  - `shake`: local, zero-LLM elision of large fenced code blocks and verbose older passages.
  - `trim [N]`: local, zero-LLM sliding window that drops older messages and keeps the last `N`.
- **History stays intact** (done). Compaction entries are appended inline on the active branch with
  their method and before/after token estimates recorded in SQLite. Stored messages are never
  rewritten or deleted, and `--cid` and `-c` resume with the full display transcript while sending
  only the active compacted context to the model.
- **Automatic compaction** (planned) will offer the same step as a conversation nears the context
  limit. It depends on a trustworthy context size, so it stays off wherever that size is unknown.

## Out of scope until after 1.0: memory tools

Basic memory tools, meaning tools that let a model read and write persistent memory across
conversations, are explicitly out of scope until after a solid 1.0 release. Proposals to add them
sooner should wait.

The **M3 Memory** milestone in the plan is a different thing. It covers the history browser,
search, branches and log import, all of which the user drives directly.

# sparkle plan

## Goal

A beautiful terminal-first chat and prompt tool with agent-quality polish. It keeps what makes
`llm` good, namely pipes, logging, templates and many models, and makes it simpler to use. It is
not an autonomous coding agent.

## Principles

1. **Two front ends, one core.** The classic CLI stays pipe-clean, and the chat interface is
   where the visual work happens.
2. **Common things get real flags.** `--effort` replaces `-o reasoning_effort`, and `-o` remains
   for everything else.
3. **Works on day one.** Existing `llm` keys, aliases and models are read, never written.
4. **Never lose the user's words.** Edits and regenerations create branches. Errors stay in the
   chat and can be retried.
5. **Graceful in every terminal.** Icons, color and motion all degrade cleanly.

## Key decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Binary name | `sparkle` | Installs alongside Python `llm` without a conflict. |
| Chat screen | Full-screen alternate screen | Editing and branching need history that can be redrawn. Native scrollback cannot be rewritten. |
| Streaming markdown | Re-render from raw source | Handles partial input and resizes with no incremental state. Open code fences skip highlighting until closed. |
| Storage | Own SQLite schema with a message tree | `parent_id` makes branching natural. `llm` logs are imported rather than shared. |
| Extensions | Native providers, no Python plugins | Compatibility is an explicit non-goal. Tools return through a simpler mechanism (below). |
| TLS and SQLite | rustls and bundled SQLite | The binary needs only glibc and libgcc at run time, which keeps packages simple. |
| Icons | `auto`, `nerd`, `unicode`, `ascii` | Ghostty, kitty and WezTerm draw Nerd Font symbols without a patched font. |

## Architecture

```text
crates/
  sparkle-core    types, config, providers, SSE, schema DSL, personas, code extraction
  sparkle-store   SQLite history with branching
  sparkle-tui     theme, icons, animation, markdown, chat state, drawing
  sparkle         command line, plain streaming, packaging metadata
```

Providers implement one trait that yields a stream of `PromptProgress`, `Reasoning`, `Text`,
`Usage` and `Finished` events. Both front ends consume the same stream.

## Feature map

Status: **done** works today, **partial** usable with gaps, **planned** has a milestone.

| `llm` feature | sparkle | Status | Milestone |
| --- | --- | --- | --- |
| Prompt, streaming, `--no-stream` | same | done | |
| `-s`, `-m`, `-o`, `--key` | same | done | |
| Reasoning on stderr, `-R` | same, with animation when hidden | done | |
| `-c`, `--cid`, `-n` | same | done | |
| `-u`, `-x`, `--xl`, `--json` | same (`--json` has fewer fields) | partial | M2 |
| `--schema`, `--schema-multi`, DSL | same | done | |
| `-a`, `--at` | same | done | |
| `-f` fragments | local files | partial | M2 |
| `--sf` system fragments | local files, repeatable | partial | M2 |
| `llm chat`, `!multi` | `sparkle chat --plain` | partial | M2 |
| `!edit`, `!fragment` | chat commands | planned | M2 |
| `-q` model search | fuzzy model picker | planned | M2 |
| `llm models`, default, options | `sparkle models` | partial | M2 |
| `llm keys` | `sparkle keys`, reads `keys.json` today | partial | M2 |
| `llm aliases` | `sparkle aliases`, reads `aliases.json` today | partial | M2 |
| `llm logs` and filters | `sparkle logs`, history browser, FTS5 search | partial | M3 |
| Log import | `sparkle import` from `logs.db` | planned | M3 |
| Templates, `-t`, `-p`, `--save` | YAML-compatible templates | planned | M3 |
| Stored fragments and schemas | `sparkle fragments`, `sparkle schemas` | planned | M3 |
| OpenAI Responses API | native provider | planned | M4 |
| Anthropic, Gemini | native providers | planned | M4 |
| Tools, `-T`, `--td`, `--ta`, `--cl` | declarative tools | planned | M5 |
| `--functions` (Python) | replaced by command tools | replaced | M5 |
| `embed`, `embed-multi`, `similar`, `collections` | same commands | planned | M6 |
| Plugins, `install`, `uninstall` | dropped by design | dropped | |

## New features

| Feature | Status |
| --- | --- |
| `--effort`, `--temp`, `--top-p`, `--max-tokens`, `--seed` | done |
| Built-in personas with `--persona` and `/persona` | done |
| Prefill with `--prefill` and `/prefill` | done |
| Edit replies with `/edit`, regenerate with `/retry` | done |
| Animated thinking, collapsible reasoning, live stats | done |
| Markdown, syntax highlighting, tables | done |
| Themes and icon tiers | done |
| User-defined personas in `~/.config/sparkle/personas/` | M2 |
| Branch navigator for edits and regenerations | M3 |
| Copy code blocks with OSC 52 | M2 |
| Pretty markdown for one-shot prompts on a terminal | M2 |
| Compare models side by side | M7 |
| Prefill progress and context meter for local `llama-server` | done |
| Context meter for remote providers | M7 |
| Manual context compaction with `/compact` (see [ROADMAP.md](ROADMAP.md)) | done |
| Automatic context compaction (see [ROADMAP.md](ROADMAP.md)) | M7 |
| Memory tools for models | after 1.0 (see [ROADMAP.md](ROADMAP.md)) |

## Tools and subagents

Tools are declared in `~/.config/sparkle/tools/*.toml`. Each one names a command, a JSON schema for
its input, and an approval policy:

```toml
name = "weather"
description = "Current weather for a city"
command = ["weather-cli", "--json"]   # receives arguments as JSON on stdin
approval = "ask"                      # ask (default), session or always
timeout_seconds = 20

[input]
city = { type = "string", description = "City name" }
```

Tool calls appear as collapsible cards in the chat, and nothing runs without approval by default.
An MCP client can follow, using the same cards and approval flow.

Subagents stay deliberately small. `/ask MODEL question` sends a side question to another model
and offers to insert the answer, and `/compare` streams one prompt to several models. There is no
autonomous planning loop.

## Milestones

| Milestone | Scope |
| --- | --- |
| **M1 Scaffold** (done) | Workspace, streaming CLI, chat interface, history, packaging |
| **M2 Daily driver** | Full CLI parity for keys, aliases and models, model picker, attachments in chat, copy, pretty one-shot output |
| **M3 Memory** | History browser, search, branches, `llm` log import, templates, stored fragments and schemas |
| **M4 Providers** | OpenAI Responses, Anthropic and Gemini native streaming |
| **M5 Tools** | Declarative tools, approval cards, tool logging, MCP client |
| **M6 Embeddings** | `embed`, `embed-multi`, `similar`, `collections` |
| **M7 Polish** | Compare view, remote context meter, automatic context compaction, theme files, accessibility pass, 1.0 packaging |

## Packaging

`scripts/package.sh` builds the release binary, generates the man page and shell completions,
and produces both packages with `cargo-generate-rpm` and `cargo-deb`.

| Item | Plan |
| --- | --- |
| Architectures | `x86_64` and `aarch64`, built natively in CI |
| Runtime dependencies | glibc and libgcc. Local builds currently require glibc 2.39 or newer, so CI should build on an older distribution for wider support. |
| Installed files | binary, man page, bash, zsh and fish completions, README |
| Release automation (M7) | Tag-triggered CI, signed packages, checksums, COPR and an apt repository |

## Testing

- Unit tests cover parsing, markdown, schema, storage and chat state.
- End-to-end tests run the real binary against an in-process mock server.
- Chat rendering is tested on ratatui's `TestBackend`.
- `tests/tui.rs` runs the real interface in a pseudo-terminal.
- `tests/flags.rs` runs every flag in every mode, and fails when a new flag has no test.

## Risks

| Risk | Mitigation |
| --- | --- |
| Prefill is not honored by every server. `bonsai2-mtp` on llama.cpp restarts its answer, giving `The answer isThe answer is 4.` | Fix first in M2: add per-model prefill capability flags, and detect or warn when a server ignores the prefill. |
| Full-screen mode loses native scrollback | Print the resume command on exit. Add transcript export in M3. |
| Provider APIs drift | Keep wire formats in small modules, each tested against recorded fixtures. |
| Long transcripts redraw slowly | Rendered turns are cached per width. Add viewport culling if profiling shows a need. |

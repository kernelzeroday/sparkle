# Study: `llm` 0.38 and prior art

Findings from running `llm` 0.38 on 2026-11-17. Behaviour was captured by pointing the real CLI
at a local mock OpenAI-compatible server that recorded every request, in an isolated config
directory.

## Observed behaviour

| Area | What `llm` does |
| --- | --- |
| Output streams | Reply text goes to stdout as it arrives. Reasoning goes to stderr. A final newline is always added. |
| `-R` | Reasoning is dropped and stderr stays empty. |
| Request shape | `{model, stream, messages, stream_options: {include_usage: true}, ...options}`. Options are top-level keys. |
| Stdin and argument | Joined as `"{stdin} {prompt}"`, so `PIPED\n` and `ARG` become `PIPED\n ARG`. |
| Fragments | Prepended to the prompt and joined with a newline. |
| Attachments | Sent as `image_url` parts with `data:` URLs. |
| Schemas | `response_format: {type: json_schema, json_schema: {name: "output", schema}}`. |
| `--schema-multi` | Wraps the schema in an object with a required `items` array. |
| `-c` | Resends the earlier system prompt and all turns, and reuses the earlier model. |
| Tool chains | Sends `tools`, runs the call, then sends the `tool` result in a follow-up request. Default chain limit is 5. |
| Usage | `-u` prints `Token usage: 11 input, 7 output` to stderr. |
| Errors | Exit code 1 with `Error: ...` on stderr. |
| Templates | YAML with `prompt`, `system`, `model`, `options`, `extract`, `schema_object`, `tools`, `functions`, `fragments`, `system_fragments`, `defaults`. `$input` and `$name` are substituted. |
| Storage | `~/.config/io.datasette.llm/`: `logs.db`, `keys.json`, `aliases.json`, `default_model.txt`, `model_options.json`, `extra-openai-models.yaml`, `templates/`. |

## Friction worth fixing

- `reasoning_effort` is only discoverable through `llm models list --options -m MODEL`.
- An API error during `llm chat` ended the session with a Python traceback. It was observed with
  a 401 response.
- `llm chat` has no history view, editing, regeneration or rendered markdown.
- Reasoning is either raw text on stderr or nothing, with no sign of progress while hidden.
- `-o` values are stringly typed, and an invalid name produces `Extra inputs are not permitted`.

## Prior art

| Project | Lesson taken |
| --- | --- |
| [Codex TUI](https://github.com/openai/codex/tree/main/codex-rs/tui) | Keep the raw source and re-render from it. Commit only newline-complete text, hold back mutable blocks such as tables and open fences, and re-render on width change. |
| [aichat](https://github.com/sigoden/aichat) | Roles, sessions and macros make a CLI feel personal. Show the active role and context in the interface. |
| [tuillem](https://lib.rs/crates/tuillem-tui) | Separate crates for markdown, providers and UI, with SQLite and FTS5 search. |
| [llm-tui](https://jakegoldsborough.com/blog/2025/building-llm-tui-terminal-ai-chat/) | ratatui, rusqlite and crossterm suit this job. Context compaction is worth planning for. |
| [Oatmeal](https://ratatui.rs/showcase/apps/) | Slash commands and chat bubbles are a familiar interaction model. |

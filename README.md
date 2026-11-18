# sparkle

A terminal-first chat and prompt tool for language models, written in Rust. It is a
reimagining of Simon Willison's [`llm`](https://llm.datasette.io/) CLI: the same pipe-friendly
command line, plus a full-screen chat interface.

sparkle is early. See [docs/PLAN.md](docs/PLAN.md) for the roadmap and what is implemented today.

## Try it

```sh
cargo run --release -p sparkle                       # open the chat interface
cargo run --release -p sparkle -- 'Capital of France?'
```

If you already use Python `llm`, sparkle reads its keys, aliases, default model and
`extra-openai-models.yaml`. It never writes to that directory.

## Command line

```sh
sparkle 'Capital of France?'             # stream a reply
sparkle --effort high 'Prove it'         # friendly flag for reasoning effort
sparkle --persona code 'Reverse a list'  # built-in system prompt
sparkle -S openai/gpt-3.5 'Hello'        # system prompt from the library, or a file path
sparkle --sf rules.md --sf tone.md 'Hi'  # build the system prompt from files
sparkle -f notes.md 'Summarize this'     # add a file to the prompt
sparkle chat -f notes.md                 # start a chat with a file attached
sparkle --prefill 'Sure:' 'Say hello'    # start the reply with your own words
git diff | sparkle 'Review this'         # pipe in context
sparkle -c 'Tell me more'                # continue the last conversation
sparkle -x 'Python hello world'          # print only the code block
sparkle --schema 'name, age int' 'A dog' # structured output
sparkle chat --plain                     # classic line-by-line chat
```

Replies go to stdout. Reasoning and animation go to stderr, so pipes receive only the reply.

## Chat keys

| Key | Action |
| --- | --- |
| `enter` | Send |
| `shift+enter`, `ctrl+j` | New line |
| `esc` | Stop the reply |
| `tab` | Complete a `/command`, press again to cycle |
| `ctrl+t` | Show or hide thinking |
| `shift+tab` | Cycle reasoning effort |
| `pgup`, `pgdn` | Scroll |
| `ctrl+c` | Stop the reply, or clear the input |
| `ctrl+c` or `ctrl+d` twice | Exit, when the input is empty |

Commands: `/help`, `/new`, `/model`, `/effort`, `/persona`, `/prompts`, `/system`, `/prefill`,
`/retry`, `/edit`, `/compact`, `/quit`.

`/compact [FOCUS]` replaces the conversation so far with a summary to free context, optionally
telling the model what to focus on. The original messages stay in the history database.

## System prompts

sparkle ships with a library of system prompts from the major model series, from GPT-3.5 and the
first Claude 3 and Gemini prompts through current releases. Open the browser with `/prompts`:

| Key | Action |
| --- | --- |
| type | Filter by name |
| `↑`, `↓`, `pgup`, `pgdn` | Move through the list |
| `←`, `→` | Switch vendor |
| `enter` | Use the highlighted prompt |
| `ctrl+d` | Make it the default for new chats, or press again to remove the default |
| `ctrl+o` | Choose an external file, with `tab` to complete paths |
| `esc` | Close |

Each row shows the prompt's estimated size and how much of the model's context window it would
fill alongside the current conversation. Prompts that reach the warning threshold are marked
`near`, and prompts that do not fit are marked `over`. The chat keeps showing a warning while the
next request is in that range, and one-shot prompts print it to stderr. Sizes are estimated at four
bytes per token. Context limits are known only for servers that report one, such as `llama-server`.

The same actions work without the browser:

```sh
sparkle prompts                          # list the library
sparkle prompts show openai/gpt-4o-2024-05
sparkle prompts default anthropic/claude-sonnet-4.5
sparkle prompts default ~/prompts/mine.md
sparkle prompts default off
```

In a chat, use `/prompts NAME`, `/prompts off`, `/prompts file PATH` and `/prompts default NAME`.
An explicit `--system`, `--system-prompt` or `--persona` takes priority over the default, and a
continued conversation keeps the system prompt it started with.

The vendored prompts belong to their vendors and are not covered by sparkle's license. See
[crates/sparkle-core/prompts/README.md](crates/sparkle-core/prompts/README.md) for where each one
came from. `scripts/vendor-prompts.py` regenerates them from `scripts/prompt-manifest.tsv`.

## Configuration

`~/.config/sparkle/config.toml` is optional:

```toml
default_model = "bonsai2-mtp"
effort = "medium"
persona = "concise"
theme = "dusk"       # dusk or paper
icons = "auto"       # auto, nerd, unicode or ascii
animations = true
show_thinking = true # show reasoning as it arrives; -R hides it for one run
smart_titles = true  # let the model name each new chat
context_warn_percent = 80 # warn when a request fills this much of the context window

[[models]]
id = "local"
api_base = "http://localhost:8080/v1"
aliases = ["l"]
```

Set `SPARKLE_REDUCED_MOTION=1` to turn animations off.

## Packaging

```sh
cargo install cargo-deb cargo-generate-rpm
./scripts/package.sh   # writes .deb and .rpm files to dist/
```

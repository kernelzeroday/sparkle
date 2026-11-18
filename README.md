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

Commands: `/help`, `/new`, `/model`, `/effort`, `/persona`, `/system`, `/prefill`, `/retry`,
`/edit`, `/compact`, `/quit`.

`/compact [FOCUS]` replaces the conversation so far with a summary to free context, optionally
telling the model what to focus on. The original messages stay in the history database.

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

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
| `enter` | Send, or queue the message while a reply is running |
| `shift+enter`, `ctrl+j` | New line |
| `esc` | Stop the reply |
| `tab` | Complete a `/command`, press again to cycle |
| `ctrl+t` | Show or hide thinking |
| `ctrl+o` | Show or hide compaction summaries |
| `shift+tab` | Cycle reasoning effort |
| `up`, `down` | Recall sent messages; `up` on an empty input takes back a queued one |
| `pgup`, `pgdn`, `shift+up`, `shift+down` | Scroll |
| `ctrl+c` | Stop the reply, or clear the input |
| `ctrl+c` or `ctrl+d` twice | Exit, when the input is empty |

Commands: `/help`, `/new`, `/model`, `/effort`, `/persona`, `/prompts`, `/system`, `/prefill`,
`/retry`, `/edit`, `/compact`, `/journal`, `/quit`.

`/compact [summary|handoff|shake|trim] [FOCUS]` inserts an inline compaction divider to free
context while keeping earlier messages visible in the scrollback (press `ctrl+o` to expand or
collapse the summary). `summary` (default) and `handoff` ask the model to condense earlier turns;
`shake` elides large code blocks locally without a model call; `trim [N]` drops older turns locally
and keeps the latest `N` messages.

### The journal

In the chat, the model keeps a journal: short notes about you that she chooses to carry from one
conversation to the next. Writing is up to her and costs nothing extra. She writes a tag anywhere
in a reply, `<journal keep="3w">…</journal>`, which is saved and hidden from the transcript; a
faint `✎ noted` marks the reply. She decides how long each note lasts (`5d`, `3w`, `6m`, `1y` or
`always`, 30 days by default), notes fade when their time is up, and `<journal forget="N"/>`
lets one go early.

The notes still in force reach her once, with the system prompt, when a conversation opens, framed
as background she need not mention or tend to. They are not refreshed mid-conversation, which keeps
server prompt caches valid, and they are capped at about 750 tokens. `/journal` shows the notes,
`/journal clear` empties it, and `journal = false` in the config turns it off. One-shot prompts and
`--plain` chats never see it.

## System prompts

sparkle ships with a library of system prompts from the major model series, from GPT-3.5 and the
first Claude 3 and Gemini prompts through current releases. Open the browser with `/prompts`:

| Key | Action |
| --- | --- |
| type | Filter by name |
| `↑`, `↓`, `pgup`, `pgdn` | Move through the list |
| `←`, `→` | Switch vendor |
| `enter` | Use the highlighted prompt, and start new chats with it too |
| `ctrl+d` | Make it the default for new chats without using it now, or press again to remove it |
| `ctrl+o` | Choose an external file, with `tab` to complete paths |
| `esc` | Close |

Each row shows the prompt's estimated size and how much of the model's context window it would
fill alongside the current conversation. Prompts that reach the warning threshold are marked
`near`, and prompts that do not fit are marked `over`. The chat keeps showing a warning while the
next request is in that range, and one-shot prompts print it to stderr. Sizes are estimated at four
bytes per token. Context limits are known only for servers that report one, such as `llama-server`.

### Local prompt pack

`scripts/build-prompt-pack.py` imports every usable system prompt from a folder of prompt
collections (`~/Documents/prompt-libraries` by default) into sparkle's data folder. Model prompts
join their vendor's tab, and the rest appear under **Uncategorized**, where `tab` switches between
source collections. The pack is never committed because its sources carry many different licenses.

The build leaves out jailbreak, sexual, roleplay and harmful content, and holds back anything that
looks like personal data. `REPORT.tsv` in the pack lists every file and the reason for its outcome.

The same actions work without the browser:

```sh
sparkle prompts                          # list the library
sparkle prompts show openai/gpt-4o-2024-05
sparkle prompts default anthropic/claude-sonnet-4.5
sparkle prompts default ~/prompts/mine.md
sparkle prompts default off
```

In a chat, use `/prompts NAME`, `/prompts off`, `/prompts file PATH` and `/prompts default NAME`.
The prompt you choose in a chat, by any of these, `/persona` or `/system TEXT`, is remembered and
new chats start with it; turning it off forgets it. Text set with `/system` is kept in
`custom_prompt.md` in the config folder. Prompts given on the command line apply to that run only.
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
show_thinking = true # show reasoning as it arrives; ctrl+t in a chat overrides this
                     # and is remembered, while -R hides it for one run
smart_titles = true  # let the model name each new chat
context_warn_percent = 80 # warn when a request fills this much of the context window
retries = 5          # resend a failed request this many times, backing off 1s, 2s, 4s…
journal = true       # let the chat model keep a journal about you (see above)

[[models]]
id = "local"
api_base = "http://localhost:8080/v1"
aliases = ["l"]
```

Set `SPARKLE_REDUCED_MOTION=1` to turn animations off.

## Packaging

Packages build offline from vendored crates. A commit tagged `v<version>` builds as that version;
any other commit builds as a snapshot such as `0.1.0^git20261119.bb3f91733`.

```sh
# Fedora and other RPM systems: SRPM and RPM in dist/rpmbuild/
sudo dnf install rpm-build rust cargo gcc gcc-c++ cmake
./scripts/build-rpm.sh                   # --without check skips the tests

# Debian and Ubuntu: source package and .deb in dist/debbuild/
sudo apt install build-essential devscripts debhelper cargo rustc cmake
./scripts/build-deb.sh                   # -d builds with a rustup toolchain if rustc < 1.89
```

Both run `scripts/make-sources.sh`, which writes the source tarball, the vendored crates, and the
license list of everything compiled into the binary to `dist/sources/`.

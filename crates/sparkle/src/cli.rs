//! Command-line definition.

use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;

const EXAMPLES: &str = "\
Examples:
  sparkle                                  Open the chat interface
  sparkle 'Capital of France?'             Ask one question
  sparkle --effort high 'Prove it'         Ask the model to think harder
  sparkle --persona code 'Reverse a list'  Use a built-in persona
  sparkle -S openai/gpt-3.5 'Hello'        Use a system prompt from the library
  git diff | sparkle 'Review this'         Pipe in context
  sparkle -c 'Tell me more'                Continue the last conversation";

#[derive(Debug, Parser)]
#[command(name = "sparkle", version, about, after_help = EXAMPLES, args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub prompt: PromptArgs,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the chat interface
    Chat(Box<ChatArgs>),
    /// List available models
    Models {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show recent conversations, or remove them
    Logs {
        /// Number of conversations to show, or 0 for all
        #[arg(short = 'n', long, default_value_t = 10)]
        count: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        action: Option<LogsAction>,
    },
    /// List built-in personas
    Personas,
    /// Browse the system prompt library and set the default
    Prompts {
        #[command(subcommand)]
        action: Option<PromptsAction>,
    },
    /// Print a shell completion script
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
    /// Print the man page
    #[command(hide = true)]
    Man,
}

#[derive(Debug, Subcommand)]
pub enum PromptsAction {
    /// List every built-in system prompt
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Print a system prompt
    Show {
        /// Library prompt, persona or file
        name: String,
    },
    /// Show or set the system prompt new chats start with
    Default {
        /// Library prompt, persona or file, or `off` to remove the default
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum LogsAction {
    /// Remove a conversation and all of its messages
    Remove {
        /// Conversation id from `sparkle logs`
        id: String,
    },
    /// Remove every conversation
    Clear,
}

#[derive(Debug, Args)]
pub struct ModelArgs {
    /// Model to use
    #[arg(short, long, help_heading = "Model")]
    pub model: Option<String>,

    /// Reasoning effort
    #[arg(short, long, value_name = "LEVEL", help_heading = "Model",
          value_parser = ["none", "minimal", "low", "medium", "high", "xhigh"])]
    pub effort: Option<String>,

    /// Sampling temperature
    #[arg(long = "temp", value_name = "FLOAT", help_heading = "Model")]
    pub temperature: Option<f64>,

    /// Nucleus sampling probability
    #[arg(long, value_name = "FLOAT", help_heading = "Model")]
    pub top_p: Option<f64>,

    /// Maximum tokens to generate
    #[arg(long, value_name = "N", help_heading = "Model")]
    pub max_tokens: Option<u64>,

    /// Seed for repeatable sampling
    #[arg(long, value_name = "N", help_heading = "Model")]
    pub seed: Option<i64>,

    /// Any other model option, such as -o verbosity low
    #[arg(short = 'o', long = "option", num_args = 2, value_names = ["KEY", "VALUE"],
          action = clap::ArgAction::Append, help_heading = "Model")]
    pub options: Vec<String>,

    /// API key, or the name of a stored key
    #[arg(long, value_name = "KEY", help_heading = "Model")]
    pub key: Option<String>,

    /// System prompt
    #[arg(short, long, value_name = "TEXT", help_heading = "Prompt")]
    pub system: Option<String>,

    /// Add a text file to the prompt, or to the first message of a chat. Can be repeated
    #[arg(short, long = "fragment", value_name = "PATH", help_heading = "Prompt")]
    pub fragments: Vec<String>,

    /// Add a text file to the system prompt. Can be repeated
    #[arg(long = "sf", visible_alias = "system-fragment", value_name = "PATH", help_heading = "Prompt")]
    pub system_fragments: Vec<String>,

    /// Built-in system prompt (see `sparkle personas`)
    #[arg(short = 'P', long, value_name = "NAME", help_heading = "Prompt")]
    pub persona: Option<String>,

    /// System prompt from the library or a file (see `sparkle prompts`)
    #[arg(short = 'S', long, value_name = "NAME", conflicts_with_all = ["system", "persona"],
          help_heading = "Prompt")]
    pub system_prompt: Option<String>,

    /// Start the reply with this text
    #[arg(long, value_name = "TEXT", help_heading = "Prompt")]
    pub prefill: Option<String>,

    /// Continue the most recent conversation
    #[arg(short, long = "continue", help_heading = "Conversation")]
    pub continue_last: bool,

    /// Continue the conversation with this ID
    #[arg(long = "cid", value_name = "ID", help_heading = "Conversation")]
    pub conversation: Option<String>,

    /// Do not save to history
    #[arg(short, long, help_heading = "Conversation")]
    pub no_log: bool,
}

#[derive(Debug, Args)]
pub struct PromptArgs {
    /// The prompt. Piped input is placed before it
    pub prompt: Option<String>,

    #[command(flatten)]
    pub model: ModelArgs,

    /// Attach a file, URL, or - for stdin
    #[arg(short, long = "attachment", value_name = "PATH", help_heading = "Prompt")]
    pub attachments: Vec<String>,

    /// Attach a file with an explicit MIME type
    #[arg(long = "at", num_args = 2, value_names = ["PATH", "MIME"],
          action = clap::ArgAction::Append, help_heading = "Prompt")]
    pub typed_attachments: Vec<String>,

    /// Request JSON matching a schema (DSL, JSON schema or file)
    #[arg(long, value_name = "SCHEMA", help_heading = "Prompt")]
    pub schema: Option<String>,

    /// Request a list of objects matching a schema
    #[arg(long, value_name = "SCHEMA", conflicts_with = "schema", help_heading = "Prompt")]
    pub schema_multi: Option<String>,

    /// Wait for the full reply instead of streaming
    #[arg(long, help_heading = "Output")]
    pub no_stream: bool,

    /// Hide reasoning and show an animation instead
    #[arg(short = 'R', long, help_heading = "Output")]
    pub hide_reasoning: bool,

    /// Show token usage
    #[arg(short, long, help_heading = "Output")]
    pub usage: bool,

    /// Print only the first fenced code block
    #[arg(short = 'x', long, help_heading = "Output")]
    pub extract: bool,

    /// Print only the last fenced code block
    #[arg(long = "xl", conflicts_with = "extract", help_heading = "Output")]
    pub extract_last: bool,

    /// Print the reply and its metadata as JSON
    #[arg(long, help_heading = "Output")]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ChatArgs {
    #[command(flatten)]
    pub model: ModelArgs,

    /// Use a plain line-by-line chat instead of the full-screen interface
    #[arg(long, help_heading = "Output")]
    pub plain: bool,

    /// Hide reasoning (ctrl+t shows it again in the chat interface)
    #[arg(short = 'R', long, help_heading = "Output")]
    pub hide_reasoning: bool,
}

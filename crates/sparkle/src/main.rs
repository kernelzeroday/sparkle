mod cli;
mod plain;

use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser};
use sparkle_core::config::{ModelEntry, Paths, Registry};
use sparkle_core::context::{self, estimate_tokens};
use sparkle_core::library::{self, Loaded};
use sparkle_core::{Attachment, ChatRequest, Message, Options, Provider, Role, extract, personas, schema};
use sparkle_store::{NewMessage, Store, StoredMessage};
use sparkle_tui::{ChatSession, Icons, ResolvedModel, Theme};
use serde_json::{Value, json};

use cli::{ChatArgs, Cli, Command, ModelArgs, PromptArgs, PromptsAction};
use plain::{Outcome, RenderOptions};

/// Requests smaller than this cannot trouble a context window, so its size is not looked up.
const CONTEXT_CHECK_BYTES: usize = 4096;
/// How long a one-shot prompt will wait to learn the model's context size.
const CONTEXT_LOOKUP: Duration = Duration::from_secs(2);

/// Settings, the model and the conversation resolved from the command line.
struct RunContext {
    paths: Paths,
    registry: Registry,
    store: Store,
    model: ModelEntry,
    provider: Arc<dyn Provider>,
    system: Option<String>,
    persona: Option<String>,
    options: Options,
    fragments: Vec<String>,
    conversation_id: Option<String>,
    history: Vec<StoredMessage>,
}

/// Thinking is shown unless `-R` or `show_thinking = false` says otherwise.
fn thinking_hidden(hide_flag: bool, registry: &Registry) -> bool {
    hide_flag || !registry.settings.show_thinking.unwrap_or(true)
}

fn parse_options(args: &ModelArgs, registry: &Registry) -> Options {
    let mut options = Options {
        effort: args.effort.clone().or_else(|| registry.settings.effort.clone()),
        temperature: args.temperature,
        top_p: args.top_p,
        max_tokens: args.max_tokens,
        seed: args.seed,
        ..Options::default()
    };
    for [key, value] in args.options.as_chunks::<2>().0 {
        let parsed = serde_json::from_str(value).unwrap_or_else(|_| json!(value));
        options.extra.insert(key.clone(), parsed);
    }
    options
}

fn prepare(args: &ModelArgs) -> Result<RunContext> {
    let paths = Paths::discover()?;
    let registry = Registry::load(&paths)?;
    let store = Store::open(&paths.database()).context("could not open the history database")?;

    let conversation_id = match (&args.conversation, args.continue_last) {
        (Some(id), _) => Some(id.clone()),
        (None, true) => {
            Some(store.latest_conversation()?.ok_or_else(|| anyhow!("no conversation to continue"))?)
        }
        (None, false) => None,
    };
    let history = match &conversation_id {
        Some(id) => {
            let thread = store.thread(id)?;
            if thread.is_empty() {
                bail!("no conversation found with ID {id}");
            }
            thread
        }
        None => Vec::new(),
    };

    let earlier_model = match &conversation_id {
        Some(id) => store.conversation_model(id)?,
        None => None,
    };
    let name =
        args.model.clone().or(earlier_model).unwrap_or_else(|| registry.default_model().to_string());
    let model = registry.resolve(&name)?;
    let provider = registry.provider(&model, args.key.as_deref());

    let inherit = args.system.is_none() && history.is_empty();
    let named = match (&args.system_prompt, &args.persona) {
        (Some(name), _) => Some(library::load(name, Some(&pack_for(&paths, &model)))?),
        (None, Some(name)) => Some(persona_prompt(name)?),
        (None, None) if inherit => inherited_prompt(&paths, &registry, &model)?,
        (None, None) => None,
    };
    let earlier_system =
        history.first().filter(|message| message.role == Role::System).map(|m| m.content.clone());
    let mut fragments = Vec::new();
    for path in &args.system_fragments {
        fragments.push(
            std::fs::read_to_string(path)
                .with_context(|| format!("could not read system fragment {path}"))?,
        );
    }
    let chosen = args.system.clone().or_else(|| named.as_ref().map(|prompt| prompt.text.clone()));
    let system = match (chosen, earlier_system) {
        (Some(system), _) => combine_system(&fragments, Some(&system)),
        // Repeating the same --sf while continuing must not stack it onto the saved prompt.
        (None, Some(earlier)) => {
            fragments.retain(|fragment| !earlier.contains(fragment.trim()));
            combine_system(&fragments, Some(&earlier))
        }
        (None, None) => combine_system(&fragments, None),
    };

    let mut prompt_fragments = Vec::new();
    for path in &args.fragments {
        prompt_fragments.push(
            std::fs::read_to_string(path).with_context(|| format!("could not read fragment {path}"))?,
        );
    }

    let options = parse_options(args, &registry);
    Ok(RunContext {
        paths,
        registry,
        store,
        model,
        provider,
        system,
        persona: named.map(|prompt| prompt.label),
        options,
        fragments: prompt_fragments,
        conversation_id,
        history,
    })
}

/// The pack a model may use. The private red-team pack is limited to local models.
fn pack_for(paths: &Paths, model: &ModelEntry) -> std::path::PathBuf {
    #[cfg(feature = "redteam")]
    if sparkle_core::openai::is_local_endpoint(&model.api_base)
        && paths.redteam_pack().join("index.tsv").is_file()
    {
        return paths.redteam_pack();
    }
    let _ = model;
    paths.prompt_pack()
}

/// The pack to list and print from, where no request is being sent.
fn browsing_pack(paths: &Paths) -> std::path::PathBuf {
    if cfg!(feature = "redteam") && paths.redteam_pack().join("index.tsv").is_file() {
        paths.redteam_pack()
    } else {
        paths.prompt_pack()
    }
}

fn persona_prompt(name: &str) -> Result<Loaded> {
    let persona = personas::find(name)
        .ok_or_else(|| anyhow!("unknown persona '{name}'. Run `sparkle personas` to see the list"))?;
    Ok(Loaded { label: persona.name.to_string(), text: persona.prompt.to_string() })
}

/// The prompt a new conversation starts with when none was asked for. A saved default that has
/// since gone missing must not stop sparkle from running.
fn inherited_prompt(paths: &Paths, registry: &Registry, model: &ModelEntry) -> Result<Option<Loaded>> {
    if let Some(name) = library::saved_default(&paths.default_prompt()) {
        match library::load(&name, Some(&pack_for(paths, model))) {
            Ok(prompt) => return Ok(Some(prompt)),
            Err(error) => eprintln!("Warning: ignoring the default system prompt: {error}"),
        }
    }
    registry.settings.persona.as_deref().map(persona_prompt).transpose()
}

/// Warns on stderr when a request is close to or beyond the model's context window.
async fn warn_about_context(context: &RunContext, messages: &[Message]) {
    let bytes: usize = messages.iter().map(|message| message.content.len()).sum();
    if bytes < CONTEXT_CHECK_BYTES {
        return;
    }
    let lookup = tokio::time::timeout(CONTEXT_LOOKUP, context.provider.context_window()).await;
    let Ok(Some(window)) = lookup else { return };
    let percent = context.registry.settings.warn_percent();
    if let Some((_, warning)) = context::warning(estimate_tokens(bytes), window, percent) {
        eprintln!("Warning: {warning}");
    }
}

/// Puts system fragments before the system prompt, separated by blank lines, as Python llm does.
fn combine_system(fragments: &[String], system: Option<&str>) -> Option<String> {
    let parts: Vec<&str> = fragments
        .iter()
        .map(String::as_str)
        .chain(system)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Joins fragments, piped input and the prompt the same way Python llm does.
fn compose_prompt(fragments: &[String], piped: Option<&str>, prompt: Option<&str>) -> String {
    let body = match (piped, prompt) {
        (Some(piped), Some(prompt)) => format!("{piped} {prompt}"),
        (Some(piped), None) => piped.to_string(),
        (None, Some(prompt)) => prompt.to_string(),
        (None, None) => String::new(),
    };
    let mut parts = fragments.to_vec();
    parts.push(body);
    parts.join("\n")
}

fn load_attachments(args: &PromptArgs, stdin_bytes: Option<&[u8]>) -> Result<Vec<Attachment>> {
    let mut attachments = Vec::new();
    for source in &args.attachments {
        let piped = (source == "-").then_some(stdin_bytes).flatten();
        attachments.push(Attachment::load(source, None, piped)?);
    }
    for [path, mime] in args.typed_attachments.as_chunks::<2>().0 {
        let piped = (path == "-").then_some(stdin_bytes).flatten();
        attachments.push(Attachment::load(path, Some(mime), piped)?);
    }
    Ok(attachments)
}

fn save_exchange(
    context: &RunContext,
    prompt: &str,
    attachments: &[Attachment],
    prefill: &str,
    outcome: &Outcome,
) -> Result<String> {
    let store = &context.store;
    let mut parent = context.history.last().map(|message| message.id.clone());
    let conversation = match &context.conversation_id {
        Some(id) => id.clone(),
        None => {
            let id = store.create_conversation(prompt, &context.model.id)?;
            if let Some(system) = &context.system {
                parent = Some(store.append(&id, NewMessage::plain(None, Role::System, system))?);
            }
            id
        }
    };
    let user_message = NewMessage {
        attachments,
        ..NewMessage::plain(parent.as_deref(), Role::User, prompt)
    };
    let user = store.append(&conversation, user_message)?;
    let content = format!("{prefill}{}", outcome.text);
    store.append(
        &conversation,
        NewMessage {
            parent_id: Some(&user),
            role: Role::Assistant,
            content: &content,
            reasoning: (!outcome.reasoning.is_empty()).then_some(outcome.reasoning.as_str()),
            attachments: &[],
            model: Some(&context.model.id),
            usage: outcome.usage,
            duration_ms: Some(outcome.duration_ms),
        },
    )?;
    Ok(conversation)
}

/// Adds a message, joining it to a user message before it because strict servers reject two in a
/// row. A compaction summary is a user message, so the next prompt always meets one.
fn push_message(messages: &mut Vec<Message>, message: Message) {
    match messages.last_mut() {
        Some(previous) if previous.role == Role::User && message.role == Role::User => {
            previous.content.push_str("\n\n");
            previous.content.push_str(&message.content);
            previous.attachments.extend(message.attachments);
        }
        _ => messages.push(message),
    }
}

fn request_messages(context: &RunContext, user: Message, prefill: Option<&str>) -> Vec<Message> {
    let mut messages = Vec::new();
    if let Some(system) = &context.system {
        messages.push(Message::new(Role::System, system.clone()));
    }
    for message in sparkle_store::active_context(&context.history) {
        push_message(&mut messages, message.to_message());
    }
    push_message(&mut messages, user);
    if let Some(prefill) = prefill {
        messages.push(Message::new(Role::Assistant, prefill));
    }
    messages
}

async fn run_prompt(args: PromptArgs) -> Result<()> {
    let context = prepare(&args.model)?;

    let wants_stdin_attachment = args.attachments.iter().any(|source| source == "-")
        || args.typed_attachments.as_chunks::<2>().0.iter().any(|[path, _]| path == "-");
    let mut stdin_bytes = Vec::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_to_end(&mut stdin_bytes).context("could not read stdin")?;
    }
    let attachments = load_attachments(&args, Some(&stdin_bytes))?;
    let piped = (!wants_stdin_attachment && !stdin_bytes.is_empty())
        .then(|| String::from_utf8_lossy(&stdin_bytes).into_owned());

    let prompt = compose_prompt(&context.fragments, piped.as_deref(), args.prompt.as_deref());
    if prompt.trim().is_empty() && attachments.is_empty() {
        bail!("no prompt given. Pass one as an argument or pipe it in");
    }

    let schema = match (&args.schema, &args.schema_multi) {
        (Some(input), _) => Some(schema::resolve(input)?),
        (None, Some(input)) => Some(schema::multi(schema::resolve(input)?)),
        (None, None) => None,
    };

    let mut user = Message::new(Role::User, prompt.clone());
    user.attachments = attachments.clone();
    let prefill = args.model.prefill.clone();
    let request = ChatRequest {
        model: context.model.wire_name.clone(),
        messages: request_messages(&context, user, prefill.as_deref()),
        options: context.options.clone(),
        schema,
        stream: !args.no_stream,
    };

    warn_about_context(&context, &request.messages).await;

    let buffered = args.json || args.extract || args.extract_last;
    let hide_reasoning = args.json || thinking_hidden(args.hide_reasoning, &context.registry);
    let render = RenderOptions { hide_reasoning, prefill: prefill.as_deref(), buffered };
    let prefix = prefill.as_deref().unwrap_or("");
    if !buffered && !prefix.is_empty() {
        print!("{prefix}");
        std::io::stdout().flush()?;
    }
    let outcome = plain::stream_reply(context.provider.as_ref(), &request, render).await?;
    let response = format!("{prefix}{}", outcome.text);

    let conversation = if args.model.no_log {
        None
    } else {
        Some(save_exchange(&context, &prompt, &attachments, prefix, &outcome)?)
    };

    if args.json {
        let usage = outcome.usage.unwrap_or_default();
        let output = json!([{
            "model": context.model.id,
            "conversation_id": conversation,
            "duration_ms": outcome.duration_ms,
            "input_tokens": outcome.usage.map(|_| usage.input_tokens),
            "output_tokens": outcome.usage.map(|_| usage.output_tokens),
            "prompt": prompt,
            "system": context.system,
            "response": response,
            "reasoning": (!outcome.reasoning.is_empty()).then_some(&outcome.reasoning),
        }]);
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if args.extract {
        print!("{}", extract::first_block(&response));
    } else if args.extract_last {
        print!("{}", extract::last_block(&response));
    } else {
        println!();
    }
    std::io::stdout().flush()?;

    if args.usage
        && let Some(usage) = outcome.usage
    {
        eprintln!("Token usage: {} input, {} output", usage.input_tokens, usage.output_tokens);
    }
    Ok(())
}

async fn run_chat(args: ChatArgs) -> Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let context = prepare(&args.model)?;
    if args.plain || !interactive {
        return run_plain_chat(context, &args).await;
    }

    let settings = context.registry.settings.clone();
    let default_prompt_file = Some(context.paths.default_prompt());
    let paths = context.paths.clone();
    let prompt_pack = Some(pack_for(&paths, &context.model));
    let show_reasoning = !thinking_hidden(args.hide_reasoning, &context.registry);
    let title = match &context.conversation_id {
        Some(id) => context.store.conversation_title(id)?,
        None => None,
    };
    let registry = context.registry;
    let key = args.model.key.clone();
    let resolver = Box::new(move |name: &str| {
        let model = registry.resolve(name).map_err(|error| error.to_string())?;
        let provider = registry.provider(&model, key.as_deref());
        let prompt_pack = Some(pack_for(&paths, &model));
        Ok(ResolvedModel { id: model.id, wire_name: model.wire_name, provider, prompt_pack })
    });
    let session = ChatSession {
        model: ResolvedModel {
            id: context.model.id,
            wire_name: context.model.wire_name,
            provider: context.provider,
            prompt_pack,
        },
        resolver,
        system: context.system,
        persona: context.persona,
        options: context.options,
        prefill: args.model.prefill.clone(),
        fragments: context.fragments,
        store: (!args.model.no_log).then_some(context.store),
        conversation_id: context.conversation_id,
        history: context.history,
        theme: Theme::named(settings.theme.as_deref()),
        icons: Icons::detect(settings.icons.as_deref()),
        animations: settings.animations.unwrap_or(true)
            && std::env::var_os("SPARKLE_REDUCED_MOTION").is_none(),
        show_reasoning,
        title,
        smart_titles: settings.smart_titles.unwrap_or(true),
        warn_percent: settings.warn_percent(),
        default_prompt_file,
    };
    if let Some(conversation) = sparkle_tui::run(session).await? {
        println!("Continue this conversation with: sparkle chat --cid {conversation}");
    }
    Ok(())
}

/// A turn kept only in memory, for chats started with --no-log.
fn unsaved_message(role: Role, content: &str) -> StoredMessage {
    StoredMessage {
        id: String::new(),
        role,
        content: content.to_string(),
        reasoning: None,
        attachments: Vec::new(),
        compacted_from: None,
        compaction: None,
    }
}

async fn run_plain_chat(mut context: RunContext, args: &ChatArgs) -> Result<()> {
    eprintln!("Chatting with {}", context.model.id);
    eprintln!("Type 'exit' or 'quit' to leave, and '!multi' to enter several lines ('!end' to finish)");
    let stdin = std::io::stdin();
    let mut multi: Option<Vec<String>> = None;
    let mut prefill = args.model.prefill.clone();
    loop {
        if multi.is_none() {
            eprint!("> ");
        }
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        let prompt = match (&mut multi, line.trim()) {
            (None, "exit" | "quit") => return Ok(()),
            (None, "!multi") => {
                multi = Some(Vec::new());
                continue;
            }
            (None, "") => continue,
            (None, _) => line,
            (Some(lines), "!end") => {
                let joined = lines.join("\n");
                multi = None;
                joined
            }
            (Some(lines), _) => {
                lines.push(line);
                continue;
            }
        };

        // Fragments accompany the first message only, and stay queued if that request fails.
        let prompt = compose_prompt(&context.fragments, None, Some(&prompt));
        let request = ChatRequest {
            model: context.model.wire_name.clone(),
            messages: request_messages(
                &context,
                Message::new(Role::User, prompt.clone()),
                prefill.as_deref(),
            ),
            options: context.options.clone(),
            schema: None,
            stream: true,
        };
        warn_about_context(&context, &request.messages).await;
        let prefix = prefill.clone().unwrap_or_default();
        print!("{prefix}");
        std::io::stdout().flush()?;
        let hide_reasoning = thinking_hidden(args.hide_reasoning, &context.registry);
        let render = RenderOptions { hide_reasoning, prefill: Some(&prefix), buffered: false };
        match plain::stream_reply(context.provider.as_ref(), &request, render).await {
            Ok(outcome) => {
                println!();
                context.fragments.clear();
                prefill = None;
                if args.model.no_log {
                    let reply = format!("{prefix}{}", outcome.text);
                    context.history.push(unsaved_message(Role::User, &prompt));
                    context.history.push(unsaved_message(Role::Assistant, &reply));
                } else {
                    let id = save_exchange(&context, &prompt, &[], &prefix, &outcome)?;
                    context.history = context.store.thread(&id)?;
                    context.conversation_id = Some(id);
                }
            }
            // A failed request should not end the conversation.
            Err(error) => eprintln!("Error: {error}"),
        }
    }
}

fn list_models(json_output: bool) -> Result<()> {
    let paths = Paths::discover()?;
    let registry = Registry::load(&paths)?;
    let default = registry.default_model();
    if json_output {
        let models: Vec<Value> = registry
            .models
            .iter()
            .map(|model| {
                json!({
                    "id": model.id,
                    "aliases": model.aliases,
                    "api_base": model.api_base,
                    "default": model.id == default,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&models)?);
        return Ok(());
    }
    for model in &registry.models {
        let marker = if model.id == default { "*" } else { " " };
        let aliases = if model.aliases.is_empty() {
            String::new()
        } else {
            format!(" (aliases: {})", model.aliases.join(", "))
        };
        println!("{marker} {}{aliases}  {}", model.id, model.api_base);
    }
    Ok(())
}

fn list_prompts(json_output: bool) -> Result<()> {
    let paths = Paths::discover()?;
    let default = library::saved_default(&paths.default_prompt());
    let pack = library::PromptPack::open(&browsing_pack(&paths));
    let built_in = library::PROMPTS.iter().map(|prompt| {
        let family = prompt.family_name();
        (prompt.id, prompt.title, family, prompt.date, prompt.origin.label(), prompt.text.len())
    });
    let packed = pack.iter().flat_map(|pack| &pack.entries).map(|entry| {
        let (id, title, group) = (entry.id.as_str(), entry.title.as_str(), entry.group.as_str());
        (id, title, group, entry.date.as_str(), entry.origin.as_str(), entry.bytes)
    });
    let prompts = built_in.chain(packed);
    if json_output {
        let rows: Vec<Value> = prompts
            .map(|(id, title, family, date, origin, bytes)| {
                json!({
                    "id": id,
                    "title": title,
                    "family": family,
                    "date": date,
                    "origin": origin,
                    "bytes": bytes,
                    "estimated_tokens": estimate_tokens(bytes),
                    "default": default.as_deref() == Some(id),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for (id, title, _, date, _, bytes) in prompts {
        let marker = if default.as_deref() == Some(id) { "*" } else { " " };
        let size = context::size_label(bytes);
        println!("{marker} {id:<40} {date:<8} {size:<20} {title}");
    }
    Ok(())
}

fn prompts_command(action: Option<PromptsAction>) -> Result<()> {
    match action.unwrap_or(PromptsAction::List { json: false }) {
        PromptsAction::List { json } => list_prompts(json),
        PromptsAction::Show { name } => {
            let pack = browsing_pack(&Paths::discover()?);
            print!("{}", library::load(&name, Some(&pack))?.text);
            Ok(())
        }
        PromptsAction::Default { name: None } => {
            match library::saved_default(&Paths::discover()?.default_prompt()) {
                Some(name) => println!("{name}"),
                None => println!("No default system prompt is set"),
            }
            Ok(())
        }
        PromptsAction::Default { name: Some(name) } => {
            let paths = Paths::discover()?;
            let file = paths.default_prompt();
            if name == library::OFF {
                library::save_default(&file, None)?;
                println!("Removed the default system prompt");
            } else {
                let stored = library::storable_name(&name, Some(&browsing_pack(&paths)))?;
                library::save_default(&file, Some(&stored))?;
                println!("New chats will start with {stored}");
            }
            Ok(())
        }
    }
}

fn show_logs(count: usize, json_output: bool) -> Result<()> {
    let paths = Paths::discover()?;
    let store = Store::open(&paths.database())?;
    let recent = store.recent(count)?;
    if json_output {
        let rows: Vec<Value> = recent
            .iter()
            .map(|row| {
                json!({
                    "id": row.id,
                    "title": row.title,
                    "model": row.model,
                    "created_ms": row.created_ms,
                    "messages": row.messages,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for row in recent {
        println!("{}  {:<16} {}", row.id, row.model, row.title);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Exit quietly when a reader such as `head` closes the pipe, like other command-line tools.
    // SAFETY: this runs before any other thread exists and only restores the default handler.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Chat(args)) => run_chat(*args).await,
        Some(Command::Models { json }) => list_models(json),
        Some(Command::Logs { count, json }) => show_logs(count, json),
        Some(Command::Personas) => {
            for persona in personas::PERSONAS {
                println!("{:<10} {}", persona.name, persona.summary);
            }
            Ok(())
        }
        Some(Command::Prompts { action }) => prompts_command(action),
        Some(Command::Completions { shell }) => {
            clap_complete::generate(shell, &mut Cli::command(), "sparkle", &mut std::io::stdout());
            Ok(())
        }
        Some(Command::Man) => {
            clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
            Ok(())
        }
        None => {
            let args = &cli.prompt;
            // These flags only apply to a one-shot prompt, so they must not open the chat.
            let one_shot = !args.attachments.is_empty()
                || !args.typed_attachments.is_empty()
                || args.schema.is_some()
                || args.schema_multi.is_some()
                || args.no_stream
                || args.usage
                || args.extract
                || args.extract_last
                || args.json;
            let bare = args.prompt.is_none() && !one_shot && std::io::stdin().is_terminal();
            if bare && std::io::stdout().is_terminal() {
                let hide_reasoning = cli.prompt.hide_reasoning;
                let args = ChatArgs { model: cli.prompt.model, plain: false, hide_reasoning };
                run_chat(args).await
            } else {
                run_prompt(cli.prompt).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_composition_matches_python_llm() {
        assert_eq!(compose_prompt(&[], Some("PIPED\n"), Some("ARG")), "PIPED\n ARG");
        assert_eq!(compose_prompt(&["FRAGTEXT\n".into()], None, Some("q")), "FRAGTEXT\n\nq");
    }

    #[test]
    fn system_fragments_combine_like_python_llm() {
        let fragments = ["ALPHA\n".to_string(), "BETA".to_string(), "  \n".to_string()];
        assert_eq!(combine_system(&fragments, None).unwrap(), "ALPHA\n\nBETA");
        assert_eq!(combine_system(&fragments, Some("SYS")).unwrap(), "ALPHA\n\nBETA\n\nSYS");
        assert_eq!(combine_system(&[], None), None);
    }

    #[test]
    fn consecutive_user_messages_are_joined_with_their_attachments() {
        let mut messages = vec![Message::new(Role::User, "summary")];
        let mut next = Message::new(Role::User, "question");
        next.attachments.push(Attachment::Url("https://example.com/a.png".into()));
        push_message(&mut messages, next);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "summary\n\nquestion");
        assert_eq!(messages[0].attachments.len(), 1);

        push_message(&mut messages, Message::new(Role::Assistant, "answer"));
        assert_eq!(messages.len(), 2);
    }

    fn model_at(api_base: &str) -> ModelEntry {
        ModelEntry {
            id: "m".into(),
            wire_name: "m".into(),
            api_base: api_base.into(),
            key_name: None,
            key_env: None,
            aliases: Vec::new(),
        }
    }

    #[test]
    fn remote_models_never_receive_the_red_team_pack() {
        let home = std::env::temp_dir().join(format!("sparkle-main-{}", std::process::id()));
        std::fs::create_dir_all(home.join("prompt-pack-redteam")).unwrap();
        std::fs::write(home.join("prompt-pack-redteam/index.tsv"), "# pack\n").unwrap();
        let paths = Paths { config_dir: home.clone(), data_dir: home.clone(), llm_dir: home.clone() };

        let remote = pack_for(&paths, &model_at("https://api.openai.com/v1"));
        assert_eq!(remote, paths.prompt_pack());
        let local = pack_for(&paths, &model_at("http://127.0.0.1:8080/v1"));
        let expected = if cfg!(feature = "redteam") { paths.redteam_pack() } else { paths.prompt_pack() };
        assert_eq!(local, expected);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }
}

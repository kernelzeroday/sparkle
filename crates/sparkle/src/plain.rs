//! Streaming output for the classic command line. Replies go to stdout, and reasoning and
//! animation go to stderr, so pipes only ever receive the reply.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::StreamExt;
use sparkle_core::prefill::PrefillFilter;
use sparkle_core::{ChatRequest, Provider, StreamEvent, Usage};
use sparkle_tui::anim;

const CLEAR_LINE: &str = "\r\x1b[2K";

#[derive(Clone, Copy, Debug)]
pub struct RenderOptions<'a> {
    pub hide_reasoning: bool,
    /// Text the reply was asked to continue from, already printed by the caller.
    pub prefill: Option<&'a str>,
    /// Collect the reply without printing it, for `--json` and `-x`.
    pub buffered: bool,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub text: String,
    pub reasoning: String,
    pub usage: Option<Usage>,
    pub duration_ms: u64,
}

/// Animated status on stderr, shown only when stderr is a terminal.
struct Spinner {
    enabled: bool,
    visible: bool,
    frame: u64,
}

impl Spinner {
    fn tick(&mut self) {
        if !self.enabled {
            return;
        }
        self.frame += 1;
        self.visible = true;
        eprint!("{CLEAR_LINE}{} Thinking", anim::spinner(self.frame));
        let _ = std::io::stderr().flush();
    }

    fn clear(&mut self) {
        if self.visible {
            eprint!("{CLEAR_LINE}");
            let _ = std::io::stderr().flush();
            self.visible = false;
        }
    }

    fn stop(&mut self) {
        self.clear();
        self.enabled = false;
    }
}

pub async fn stream_reply(
    provider: &dyn Provider,
    request: &ChatRequest,
    render: RenderOptions<'_>,
) -> Result<Outcome> {
    let mut filter = PrefillFilter::new(render.prefill.unwrap_or(""));
    let started = Instant::now();
    let stderr_is_terminal = std::io::stderr().is_terminal();
    let mut spinner = Spinner { enabled: stderr_is_terminal, visible: false, frame: 0 };
    let mut outcome = Outcome::default();
    let mut ticker = tokio::time::interval(Duration::from_millis(80));
    let mut stdout = std::io::stdout();
    let mut reasoning_open = false;

    let connect = provider.stream(request);
    tokio::pin!(connect);
    let mut stream = loop {
        tokio::select! {
            result = &mut connect => match result {
                Ok(stream) => break stream,
                Err(error) => {
                    spinner.stop();
                    return Err(error.into());
                }
            },
            _ = ticker.tick() => spinner.tick(),
        }
    };

    loop {
        let event = tokio::select! {
            event = stream.next() => event,
            _ = ticker.tick() => {
                spinner.tick();
                continue;
            }
        };
        let Some(event) = event else { break };
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                spinner.stop();
                return Err(error.into());
            }
        };
        match event {
            StreamEvent::Reasoning(text) => {
                outcome.reasoning.push_str(&text);
                if !render.hide_reasoning {
                    spinner.stop();
                    reasoning_open = true;
                    if stderr_is_terminal {
                        eprint!("\x1b[2m{text}\x1b[0m");
                    } else {
                        eprint!("{text}");
                    }
                    let _ = std::io::stderr().flush();
                }
            }
            StreamEvent::Text(text) => {
                spinner.stop();
                if reasoning_open {
                    eprintln!("\n");
                    reasoning_open = false;
                }
                let text = filter.push(&text);
                outcome.text.push_str(&text);
                if !render.buffered {
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                }
            }
            StreamEvent::Usage(usage) => outcome.usage = Some(usage),
            StreamEvent::Finished(_) => {}
        }
    }
    spinner.stop();
    if reasoning_open {
        eprintln!();
    }
    let held_back = filter.finish();
    outcome.text.push_str(&held_back);
    if !render.buffered {
        stdout.write_all(held_back.as_bytes())?;
        stdout.flush()?;
    }
    outcome.duration_ms = started.elapsed().as_millis() as u64;
    Ok(outcome)
}

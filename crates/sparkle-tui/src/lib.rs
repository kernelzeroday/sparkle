//! The full-screen chat interface.

pub mod anim;
pub mod app;
pub mod markdown;
pub mod theme;
pub mod view;

use std::io::{self, Write, stdout};
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{SetTitle, supports_keyboard_enhancement};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

pub use app::{App, ChatSession, ResolvedModel, Resolver};
pub use theme::{Icons, Theme};

use app::Action;

const FRAME: Duration = Duration::from_millis(80);
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Reads terminal input on its own thread so it can be stopped cleanly while an editor runs.
struct InputThread {
    events: UnboundedReceiver<io::Result<Event>>,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl InputThread {
    fn start() -> Self {
        let (sender, events) = unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let event = match event::poll(INPUT_POLL) {
                    Ok(true) => event::read(),
                    Ok(false) => continue,
                    Err(error) => Err(error),
                };
                let failed = event.is_err();
                if sender.send(event).is_err() || failed {
                    break;
                }
            }
        });
        Self { events, stop, handle }
    }

    /// Stops reading, so that an editor can have the keyboard to itself.
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

fn enter_terminal() -> io::Result<(DefaultTerminal, bool)> {
    let terminal = ratatui::try_init()?;
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    // Saves the current window title so it can be put back on exit.
    stdout().write_all(b"\x1b[22;2t")?;
    execute!(stdout(), EnableBracketedPaste)?;
    if enhanced {
        // Lets the terminal report shift+enter separately from enter.
        execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    Ok((terminal, enhanced))
}

fn leave_terminal(enhanced: bool) {
    if enhanced {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout(), DisableBracketedPaste);
    let _ = stdout().write_all(b"\x1b[23;2t");
    let _ = stdout().flush();
    ratatui::restore();
}

/// Opens text in the user's editor and returns the saved result.
fn edit_in_editor(text: &str) -> io::Result<String> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_nanos());
    let path = std::env::temp_dir().join(format!("sparkle-edit-{}-{stamp}.md", std::process::id()));
    let mut file =
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    file.write_all(text.as_bytes())?;
    drop(file);

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sparkle-editor")
        .arg(&path)
        .status();
    let result = status.and_then(|status| {
        if status.success() {
            std::fs::read_to_string(&path)
        } else {
            Err(io::Error::other(format!("the editor exited with {status}")))
        }
    });
    let _ = std::fs::remove_file(&path);
    result
}

/// Runs the chat until the user quits. Returns the conversation id, if one was saved.
pub async fn run(session: ChatSession) -> io::Result<Option<String>> {
    let (mut app, mut receiver) = App::new(session);
    app.request_context_window();
    let (mut terminal, enhanced) = enter_terminal()?;
    let mut input = InputThread::start();
    let mut ticker = tokio::time::interval(FRAME);

    let mut shown_title = String::new();

    let outcome = loop {
        let title = app.window_title();
        if title != shown_title {
            let _ = execute!(stdout(), SetTitle(&title));
            shown_title = title;
        }
        if app.dirty {
            app.dirty = false;
            if let Err(error) = terminal.draw(|frame| view::draw(frame, &mut app)) {
                break Err(error);
            }
        }
        tokio::select! {
            event = input.events.recv() => match event {
                Some(Ok(event)) => app.on_terminal(event),
                Some(Err(error)) => break Err(error),
                None => break Ok(()),
            },
            Some(message) = receiver.recv() => {
                app.on_stream(message);
                while let Ok(message) = receiver.try_recv() {
                    app.on_stream(message);
                }
            }
            _ = ticker.tick(), if app.busy() || app.exit_pending() => {
                app.expire_exit();
                if app.busy() {
                    app.frame = app.frame.wrapping_add(1);
                    app.dirty = true;
                }
            }
        }
        if app.action.take() == Some(Action::EditLastReply) {
            // The reader must stop first or it competes with the editor for keystrokes.
            input.stop();
            leave_terminal(enhanced);
            let text = app.last_reply_text().unwrap_or_default().to_string();
            let edited = edit_in_editor(&text);
            match enter_terminal() {
                Ok((restored, _)) => terminal = restored,
                Err(error) => return Err(error),
            }
            input = InputThread::start();
            shown_title.clear();
            match edited {
                Ok(text) => app.apply_edit(text),
                Err(error) => app.notice = Some(format!("Could not edit: {error}")),
            }
            app.dirty = true;
        }
        if app.should_quit {
            break Ok(());
        }
    };

    input.stop();
    app.cancel();
    leave_terminal(enhanced);
    outcome.map(|()| app.conversation_id.clone())
}

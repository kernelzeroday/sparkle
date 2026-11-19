//! Copying to the system clipboard. Over SSH the terminal's own clipboard sequence (OSC 52) is
//! used, so the text lands on the person's machine rather than the server; locally a native tool
//! (wl-copy, xclip, xsel, pbcopy) is used when one is installed, with OSC 52 as the fallback.
//! `SPARKLE_CLIPBOARD` forces one: osc52, wl-copy, xclip, xsel, pbcopy or none.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use base64::Engine;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// The terminal's clipboard escape sequence.
    Osc52,
    /// A program that reads the text on stdin.
    Program { name: &'static str, args: &'static [&'static str] },
    /// Copying is turned off.
    Disabled,
}

const WL_COPY: Method = Method::Program { name: "wl-copy", args: &[] };
const XCLIP: Method = Method::Program { name: "xclip", args: &["-selection", "clipboard"] };
const XSEL: Method = Method::Program { name: "xsel", args: &["--clipboard", "--input"] };
const PBCOPY: Method = Method::Program { name: "pbcopy", args: &[] };

/// Picks how to copy from environment variables (`var`) and installed programs (`installed`).
pub fn choose(var: impl Fn(&str) -> Option<String>, installed: impl Fn(&str) -> bool) -> Method {
    let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    match var("SPARKLE_CLIPBOARD").as_deref() {
        Some("osc52") => return Method::Osc52,
        Some("none" | "off") => return Method::Disabled,
        Some("wl-copy") => return WL_COPY,
        Some("xclip") => return XCLIP,
        Some("xsel") => return XSEL,
        Some("pbcopy") => return PBCOPY,
        _ => {}
    }
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return Method::Osc52;
    }
    let candidates: &[(bool, Method)] = &[
        (set("WAYLAND_DISPLAY"), WL_COPY),
        (set("DISPLAY"), XCLIP),
        (set("DISPLAY"), XSEL),
        (cfg!(target_os = "macos"), PBCOPY),
    ];
    for (usable, method) in candidates {
        if let Method::Program { name, .. } = method
            && *usable
            && installed(name)
        {
            return method.clone();
        }
    }
    Method::Osc52
}

/// The method for this process, from its real environment and PATH.
pub fn detect() -> Method {
    choose(|name| std::env::var(name).ok(), on_path)
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| Path::new(&dir).join(program).is_file())
    })
}

/// The OSC 52 sequence that asks the terminal to put `text` on the clipboard. Inside tmux it is
/// wrapped so that tmux passes it through to the outer terminal.
pub fn osc52(text: &str, in_tmux: bool) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    if in_tmux {
        format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
    } else {
        sequence
    }
}

/// Copies `text`. A program runs on its own thread so a slow clipboard never holds up the chat;
/// if it cannot be started, the terminal sequence is used instead.
pub fn copy(method: &Method, text: &str, terminal: &mut impl Write) -> std::io::Result<()> {
    match method {
        Method::Disabled => Ok(()),
        Method::Osc52 => {
            let in_tmux = std::env::var_os("TMUX").is_some();
            terminal.write_all(osc52(text, in_tmux).as_bytes())?;
            terminal.flush()
        }
        Method::Program { name, args } => {
            let child = Command::new(name)
                .args(args.iter())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match child {
                Ok(mut child) => {
                    let text = text.to_string();
                    std::thread::spawn(move || {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(text.as_bytes());
                        }
                        let _ = child.wait();
                    });
                    Ok(())
                }
                Err(_) => copy(&Method::Osc52, text, terminal),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
    }

    #[test]
    fn local_sessions_use_a_native_tool_and_ssh_uses_the_terminal() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        assert_eq!(choose(env(&[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]), all), WL_COPY);
        assert_eq!(choose(env(&[("DISPLAY", ":0")]), all), XCLIP);
        assert_eq!(choose(env(&[("DISPLAY", ":0")]), |name| name == "xsel"), XSEL);
        // Over SSH a native tool would fill the server's clipboard, not the person's.
        let ssh = [("WAYLAND_DISPLAY", "wayland-0"), ("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22")];
        assert_eq!(choose(env(&ssh), all), Method::Osc52);
        // Nothing installed falls back to the terminal.
        assert_eq!(choose(env(&[("WAYLAND_DISPLAY", "wayland-0")]), none), Method::Osc52);
    }

    #[test]
    fn the_method_can_be_forced() {
        let all = |_: &str| true;
        let wayland = |value: &'static str| [("WAYLAND_DISPLAY", "wayland-0"), ("SPARKLE_CLIPBOARD", value)];
        assert_eq!(choose(env(&wayland("osc52")), all), Method::Osc52);
        assert_eq!(choose(env(&wayland("none")), all), Method::Disabled);
        assert_eq!(choose(env(&wayland("xsel")), all), XSEL);
    }

    #[test]
    fn osc52_carries_the_text_in_base64_and_passes_through_tmux() {
        assert_eq!(osc52("hi", false), "\x1b]52;c;aGk=\x07");
        assert_eq!(osc52("hi", true), "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
        let mut out = Vec::new();
        copy(&Method::Osc52, "hi", &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("]52;c;aGk="));
        let mut nothing = Vec::new();
        copy(&Method::Disabled, "hi", &mut nothing).unwrap();
        assert!(nothing.is_empty());
    }
}

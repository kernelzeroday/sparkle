//! Colors and icon sets.

use ratatui::style::Color;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub accent: Color,
    pub user: Color,
    pub heading: Color,
    pub link: Color,
    pub inline_code: Color,
    pub code_bg: Color,
    pub error: Color,
    pub shimmer_low: (u8, u8, u8),
    pub shimmer_high: (u8, u8, u8),
    pub syntax_theme: &'static str,
}

impl Theme {
    pub const DUSK: Theme = Theme {
        text: Color::Rgb(224, 222, 244),
        muted: Color::Rgb(144, 140, 170),
        faint: Color::Rgb(86, 82, 110),
        accent: Color::Rgb(196, 167, 231),
        user: Color::Rgb(235, 188, 186),
        heading: Color::Rgb(246, 193, 119),
        link: Color::Rgb(156, 207, 216),
        inline_code: Color::Rgb(156, 207, 216),
        code_bg: Color::Rgb(31, 29, 46),
        error: Color::Rgb(235, 111, 146),
        shimmer_low: (110, 106, 134),
        shimmer_high: (235, 225, 255),
        syntax_theme: "base16-ocean.dark",
    };

    pub const PAPER: Theme = Theme {
        text: Color::Rgb(52, 48, 70),
        muted: Color::Rgb(110, 106, 134),
        faint: Color::Rgb(170, 166, 185),
        accent: Color::Rgb(144, 122, 169),
        user: Color::Rgb(180, 99, 122),
        heading: Color::Rgb(180, 120, 30),
        link: Color::Rgb(40, 105, 131),
        inline_code: Color::Rgb(40, 105, 131),
        code_bg: Color::Rgb(242, 233, 225),
        error: Color::Rgb(180, 50, 90),
        shimmer_low: (170, 166, 185),
        shimmer_high: (70, 50, 110),
        syntax_theme: "InspiredGitHub",
    };

    pub fn named(name: Option<&str>) -> Theme {
        match name {
            Some("paper" | "light") => Theme::PAPER,
            _ => Theme::DUSK,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icons {
    pub user: &'static str,
    pub assistant: &'static str,
    pub thinking: &'static str,
    pub code: &'static str,
    pub model: &'static str,
    pub effort: &'static str,
    pub persona: &'static str,
    pub error: &'static str,
    pub stopped: &'static str,
    pub collapsed: &'static str,
    pub expanded: &'static str,
    pub bullet: &'static str,
}

impl Icons {
    pub const NERD: Icons = Icons {
        user: "\u{f007}",
        assistant: "\u{f0674}",
        thinking: "\u{f09d1}",
        code: "\u{f121}",
        model: "\u{f061a}",
        effort: "\u{f029a}",
        persona: "\u{f0004}",
        error: "\u{f057}",
        stopped: "\u{f04d}",
        collapsed: "\u{f054}",
        expanded: "\u{f078}",
        bullet: "•",
    };

    pub const UNICODE: Icons = Icons {
        user: "❯",
        assistant: "✦",
        thinking: "◆",
        code: "⌘",
        model: "◈",
        effort: "▲",
        persona: "♥",
        error: "✖",
        stopped: "■",
        collapsed: "▸",
        expanded: "▾",
        bullet: "•",
    };

    pub const ASCII: Icons = Icons {
        user: ">",
        assistant: "*",
        thinking: "~",
        code: "#",
        model: "@",
        effort: "^",
        persona: "&",
        error: "x",
        stopped: "-",
        collapsed: ">",
        expanded: "v",
        bullet: "-",
    };

    /// Chooses from the `icons` setting, or from the terminal when it is `auto` or unset.
    /// Ghostty, kitty and WezTerm draw Nerd Font symbols without a patched font installed.
    pub fn detect(setting: Option<&str>) -> Icons {
        let term = std::env::var("TERM").unwrap_or_default();
        let program = std::env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
        Self::choose(setting, &term, &program)
    }

    fn choose(setting: Option<&str>, term: &str, program: &str) -> Icons {
        match setting {
            Some("nerd") => return Icons::NERD,
            Some("unicode") => return Icons::UNICODE,
            Some("ascii") => return Icons::ASCII,
            _ => {}
        }
        if term == "linux" || term == "dumb" {
            return Icons::ASCII;
        }
        let bundled = ["ghostty", "kitty", "wezterm"];
        if bundled.iter().any(|name| term.contains(name) || program.contains(name)) {
            Icons::NERD
        } else {
            Icons::UNICODE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn themes_are_chosen_by_name() {
        assert_eq!(Theme::named(Some("paper")).syntax_theme, Theme::PAPER.syntax_theme);
        assert_eq!(Theme::named(Some("light")).syntax_theme, Theme::PAPER.syntax_theme);
        assert_eq!(Theme::named(Some("other")).syntax_theme, Theme::DUSK.syntax_theme);
        assert_eq!(Theme::named(None).syntax_theme, Theme::DUSK.syntax_theme);
    }

    #[test]
    fn detection_respects_setting_then_terminal() {
        assert_eq!(Icons::choose(Some("ascii"), "xterm-ghostty", ""), Icons::ASCII);
        assert_eq!(Icons::choose(None, "xterm-ghostty", ""), Icons::NERD);
        assert_eq!(Icons::choose(Some("auto"), "xterm-256color", "wezterm"), Icons::NERD);
        assert_eq!(Icons::choose(None, "xterm-256color", ""), Icons::UNICODE);
        assert_eq!(Icons::choose(None, "linux", ""), Icons::ASCII);
        assert_eq!(Icons::choose(Some("nerd"), "linux", ""), Icons::NERD);
        assert_eq!(Icons::choose(Some("unicode"), "xterm-ghostty", ""), Icons::UNICODE);
    }
}

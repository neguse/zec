//! Conservative capability detection from the environment.
//!
//! Nothing is probed at runtime; the detection only reads variables the
//! terminal or multiplexer sets, and `ZEC_KEYBOARD_PROTOCOL` overrides it.

use std::{env, ffi::OsString};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardProtocol {
    Kitty,
    ModifyOtherKeys,
    Legacy,
}

impl KeyboardProtocol {
    fn label(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::ModifyOtherKeys => "modifyOtherKeys",
            Self::Legacy => "legacy+F1",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorCapability {
    TrueColor,
    Ansi256,
    Ansi16,
}

impl ColorCapability {
    fn label(self) -> &'static str {
        match self {
            Self::TrueColor => "truecolor",
            Self::Ansi256 => "256",
            Self::Ansi16 => "16",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub keyboard: KeyboardProtocol,
    pub color: ColorCapability,
    pub utf8: bool,
    pub mouse_requested: bool,
    pub focus_requested: bool,
    pub osc52_attempted: bool,
    pub mouse_observed: bool,
    pub focus_observed: bool,
}

impl Capabilities {
    pub fn detect() -> Self {
        Self::detect_with(|name| env::var_os(name))
    }

    fn detect_with(mut value: impl FnMut(&str) -> Option<OsString>) -> Self {
        let text = |name: &str, value: &mut dyn FnMut(&str) -> Option<OsString>| {
            value(name).map(|value| value.to_string_lossy().to_ascii_lowercase())
        };
        let term = text("TERM", &mut value).unwrap_or_default();
        let term_program = text("TERM_PROGRAM", &mut value).unwrap_or_default();
        let override_protocol = text("ZEC_KEYBOARD_PROTOCOL", &mut value);
        let inside_tmux = value("TMUX").is_some();
        let kitty_environment = value("KITTY_WINDOW_ID").is_some()
            || value("WEZTERM_PANE").is_some()
            || term.contains("kitty")
            || term.starts_with("foot")
            || matches!(term_program.as_str(), "wezterm" | "ghostty");
        let modify_other_keys_environment = value("XTERM_VERSION").is_some()
            || term.starts_with("xterm")
            || matches!(term_program.as_str(), "iterm.app" | "apple_terminal");
        let keyboard = match override_protocol.as_deref() {
            Some("kitty") => KeyboardProtocol::Kitty,
            Some("modifyotherkeys" | "modify_other_keys" | "xterm") => {
                KeyboardProtocol::ModifyOtherKeys
            }
            Some("legacy") => KeyboardProtocol::Legacy,
            _ if kitty_environment && !inside_tmux => KeyboardProtocol::Kitty,
            _ if modify_other_keys_environment && !inside_tmux => KeyboardProtocol::ModifyOtherKeys,
            _ => KeyboardProtocol::Legacy,
        };
        // Crossterm cannot push keyboard enhancement flags through the
        // Windows console API; requesting kitty there aborts startup.
        let keyboard = if cfg!(windows) && keyboard == KeyboardProtocol::Kitty {
            KeyboardProtocol::Legacy
        } else {
            keyboard
        };
        let color_term = text("COLORTERM", &mut value).unwrap_or_default();
        let color = if matches!(color_term.as_str(), "truecolor" | "24bit") {
            ColorCapability::TrueColor
        } else if term.contains("256color") {
            ColorCapability::Ansi256
        } else {
            ColorCapability::Ansi16
        };
        let locale = text("LC_ALL", &mut value)
            .filter(|locale| !locale.is_empty())
            .or_else(|| text("LC_CTYPE", &mut value).filter(|locale| !locale.is_empty()))
            .or_else(|| text("LANG", &mut value))
            .unwrap_or_default();
        let usable_terminal = term != "dumb";

        Self {
            keyboard,
            color,
            utf8: locale.contains("utf-8") || locale.contains("utf8"),
            mouse_requested: usable_terminal,
            focus_requested: usable_terminal,
            osc52_attempted: usable_terminal,
            mouse_observed: false,
            focus_observed: false,
        }
    }

    pub fn observe_mouse(&mut self) {
        self.mouse_observed = true;
    }

    pub fn observe_focus(&mut self) {
        self.focus_observed = true;
    }

    pub fn summary(&self) -> String {
        let seen = |observed: bool| if observed { "seen" } else { "unverified" };
        let on = |requested: bool| if requested { "on" } else { "off" };
        format!(
            "terminal keyboard={} mouse={}/{} focus={}/{} OSC52={} color={} UTF-8={}",
            self.keyboard.label(),
            on(self.mouse_requested),
            seen(self.mouse_observed),
            on(self.focus_requested),
            seen(self.focus_observed),
            if self.osc52_attempted {
                "attempt"
            } else {
                "off"
            },
            self.color.label(),
            if self.utf8 { "yes" } else { "no" },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn detect(pairs: &[(&str, &str)]) -> Capabilities {
        let values = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect::<BTreeMap<_, _>>();
        Capabilities::detect_with(|name| values.get(name).cloned())
    }

    #[test]
    fn detection_is_conservative_and_overrideable() {
        let expected_kitty = if cfg!(windows) {
            KeyboardProtocol::Legacy
        } else {
            KeyboardProtocol::Kitty
        };

        let kitty = detect(&[
            ("TERM", "xterm-kitty"),
            ("COLORTERM", "truecolor"),
            ("LANG", "C.UTF-8"),
            ("KITTY_WINDOW_ID", "1"),
        ]);
        assert_eq!(kitty.keyboard, expected_kitty);
        assert_eq!(kitty.color, ColorCapability::TrueColor);
        assert!(kitty.utf8 && kitty.mouse_requested && kitty.focus_requested);

        let multiplexed = detect(&[
            ("TERM", "xterm-kitty"),
            ("KITTY_WINDOW_ID", "1"),
            ("TMUX", "/tmp/tmux"),
        ]);
        assert_eq!(multiplexed.keyboard, KeyboardProtocol::Legacy);

        let overridden = detect(&[("TERM", "dumb"), ("ZEC_KEYBOARD_PROTOCOL", "kitty")]);
        assert_eq!(overridden.keyboard, expected_kitty);
        assert!(!overridden.mouse_requested);
        assert!(!overridden.focus_requested);
        assert!(!overridden.osc52_attempted);
    }
}

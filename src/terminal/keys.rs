//! Crossterm key events to GPUI keystrokes.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers as CrosstermModifiers};
use gpui::{Keystroke, Modifiers as GpuiModifiers};

/// Converts a key press into the keystroke Zed's keymap understands.
///
/// Release events and keys GPUI has no name for produce `None`.
pub fn to_gpui_keystroke(event: KeyEvent) -> Option<Keystroke> {
    if event.kind == KeyEventKind::Release {
        return None;
    }

    let mut modifiers = GpuiModifiers {
        control: event.modifiers.contains(CrosstermModifiers::CONTROL),
        alt: event
            .modifiers
            .intersects(CrosstermModifiers::ALT | CrosstermModifiers::META),
        shift: event.modifiers.contains(CrosstermModifiers::SHIFT),
        platform: event.modifiers.contains(CrosstermModifiers::SUPER),
        function: false,
    };

    let (key, key_char) = match event.code {
        KeyCode::Backspace => ("backspace".to_owned(), None),
        KeyCode::Enter => ("enter".to_owned(), None),
        KeyCode::Left => ("left".to_owned(), None),
        KeyCode::Right => ("right".to_owned(), None),
        KeyCode::Up => ("up".to_owned(), None),
        KeyCode::Down => ("down".to_owned(), None),
        KeyCode::Home => ("home".to_owned(), None),
        KeyCode::End => ("end".to_owned(), None),
        KeyCode::PageUp => ("pageup".to_owned(), None),
        KeyCode::PageDown => ("pagedown".to_owned(), None),
        KeyCode::Tab => ("tab".to_owned(), None),
        KeyCode::BackTab => {
            modifiers.shift = true;
            ("tab".to_owned(), None)
        }
        KeyCode::Delete => ("delete".to_owned(), None),
        KeyCode::Insert => ("insert".to_owned(), None),
        KeyCode::F(number) => (format!("f{number}"), None),
        KeyCode::Esc => ("escape".to_owned(), None),
        KeyCode::Menu => ("menu".to_owned(), None),
        KeyCode::Char(character) => {
            let key = if character == ' ' {
                "space".to_owned()
            } else {
                character.to_lowercase().collect()
            };

            // Ctrl/Super shortcuts must not fall through to printable input.
            // Ctrl+Alt can be AltGr, so its character input is preserved.
            let key_char = (!character.is_control()
                && !modifiers.platform
                && !modifiers.function
                && (!modifiers.control || modifiers.alt))
                .then(|| character.to_string());

            (key, key_char)
        }
        _ => return None,
    };

    // Match GPUI's Linux keyboard normalization for shifted punctuation.
    if modifiers.shift && key.chars().count() == 1 && key.to_lowercase() == key.to_uppercase() {
        modifiers.shift = false;
    }

    Some(Keystroke {
        modifiers,
        key,
        key_char,
    })
}

/// Formats a keystroke the way the status row and palette show it.
pub fn display_keystroke(keystroke: &Keystroke) -> String {
    let mut parts = Vec::new();
    if keystroke.modifiers.control {
        parts.push("Ctrl".to_owned());
    }
    if keystroke.modifiers.alt {
        parts.push("Alt".to_owned());
    }
    if keystroke.modifiers.shift {
        parts.push("Shift".to_owned());
    }
    if keystroke.modifiers.platform {
        parts.push("Super".to_owned());
    }
    let key = match keystroke.key.as_str() {
        "pageup" => "PgUp".to_owned(),
        "pagedown" => "PgDn".to_owned(),
        "escape" => "Esc".to_owned(),
        key => {
            let mut characters = key.chars();
            match characters.next() {
                Some(first) => first.to_uppercase().chain(characters).collect(),
                None => String::new(),
            }
        }
    };
    parts.push(key);
    parts.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_printable_characters() {
        let stroke =
            to_gpui_keystroke(KeyEvent::new(KeyCode::Char('A'), CrosstermModifiers::SHIFT))
                .unwrap();
        assert_eq!(stroke.key, "a");
        assert_eq!(stroke.key_char.as_deref(), Some("A"));
        assert!(stroke.modifiers.shift);
    }

    #[test]
    fn control_shortcuts_carry_no_character() {
        let stroke = to_gpui_keystroke(KeyEvent::new(
            KeyCode::Char('s'),
            CrosstermModifiers::CONTROL,
        ))
        .unwrap();
        assert_eq!(stroke.key, "s");
        assert!(stroke.key_char.is_none());
        assert!(stroke.modifiers.control);
    }

    #[test]
    fn shifted_punctuation_drops_the_shift_modifier() {
        let stroke =
            to_gpui_keystroke(KeyEvent::new(KeyCode::Char(':'), CrosstermModifiers::SHIFT))
                .unwrap();
        assert_eq!(stroke.key, ":");
        assert!(!stroke.modifiers.shift);
    }

    #[test]
    fn releases_are_ignored() {
        let mut event = KeyEvent::new(KeyCode::Char('a'), CrosstermModifiers::NONE);
        event.kind = KeyEventKind::Release;
        assert!(to_gpui_keystroke(event).is_none());
    }

    #[test]
    fn displays_keystrokes_for_humans() {
        let stroke = Keystroke::parse("ctrl-pagedown").unwrap();
        assert_eq!(display_keystroke(&stroke), "Ctrl-PgDn");
        let stroke = Keystroke::parse("f1").unwrap();
        assert_eq!(display_keystroke(&stroke), "F1");
    }
}

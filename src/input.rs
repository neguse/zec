use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers as CrosstermModifiers};
use gpui::{Keystroke, Modifiers as GpuiModifiers};

pub fn is_quit(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('q')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_save(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('s')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_find(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('f')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_copy(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('c')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_cut(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('x')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_intercepted_shortcut(event: &KeyEvent) -> bool {
    event.modifiers == CrosstermModifiers::CONTROL
        && matches!(event.code, KeyCode::Char('c' | 'f' | 'q' | 's' | 'x'))
}

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

            // Ctrl/Super shortcuts must not fall through to printable input. Ctrl+Alt can be
            // AltGr, so preserve its character input.
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
    fn does_not_insert_control_shortcuts() {
        let stroke = to_gpui_keystroke(KeyEvent::new(
            KeyCode::Char('z'),
            CrosstermModifiers::CONTROL,
        ))
        .unwrap();

        assert_eq!(stroke.key, "z");
        assert_eq!(stroke.key_char, None);
        assert!(stroke.modifiers.control);
    }

    #[test]
    fn normalizes_backtab() {
        let stroke =
            to_gpui_keystroke(KeyEvent::new(KeyCode::BackTab, CrosstermModifiers::NONE)).unwrap();

        assert_eq!(stroke.key, "tab");
        assert!(stroke.modifiers.shift);
    }

    #[test]
    fn ignores_release_events() {
        let event = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            CrosstermModifiers::NONE,
            KeyEventKind::Release,
        );

        assert!(to_gpui_keystroke(event).is_none());
    }

    #[test]
    fn recognizes_only_plain_control_q_as_quit() {
        assert!(is_quit(&KeyEvent::new(
            KeyCode::Char('q'),
            CrosstermModifiers::CONTROL,
        )));
        assert!(!is_quit(&KeyEvent::new(
            KeyCode::Char('q'),
            CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
        )));
        assert!(!is_quit(&KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Repeat,
        )));
        assert!(!is_quit(&KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Release,
        )));
    }

    #[test]
    fn recognizes_only_plain_control_s_press_as_save() {
        assert!(is_save(&KeyEvent::new(
            KeyCode::Char('s'),
            CrosstermModifiers::CONTROL,
        )));
        assert!(!is_save(&KeyEvent::new(
            KeyCode::Char('s'),
            CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
        )));
        assert!(!is_save(&KeyEvent::new_with_kind(
            KeyCode::Char('s'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Repeat,
        )));
        assert!(!is_save(&KeyEvent::new_with_kind(
            KeyCode::Char('s'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Release,
        )));
    }

    #[test]
    fn recognizes_only_plain_control_f_press_as_find() {
        assert!(is_find(&KeyEvent::new(
            KeyCode::Char('f'),
            CrosstermModifiers::CONTROL,
        )));
        assert!(!is_find(&KeyEvent::new(
            KeyCode::Char('f'),
            CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
        )));
        assert!(!is_find(&KeyEvent::new_with_kind(
            KeyCode::Char('f'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Repeat,
        )));
        assert!(!is_find(&KeyEvent::new_with_kind(
            KeyCode::Char('f'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Release,
        )));
    }

    #[test]
    fn recognizes_only_plain_control_c_press_as_copy() {
        assert_plain_control_press_only(is_copy, 'c');
    }

    #[test]
    fn recognizes_only_plain_control_x_press_as_cut() {
        assert_plain_control_press_only(is_cut, 'x');
    }

    fn assert_plain_control_press_only(recognizer: fn(&KeyEvent) -> bool, character: char) {
        assert!(recognizer(&KeyEvent::new(
            KeyCode::Char(character),
            CrosstermModifiers::CONTROL,
        )));
        assert!(!recognizer(&KeyEvent::new(
            KeyCode::Char(character),
            CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
        )));
        assert!(!recognizer(&KeyEvent::new(
            KeyCode::Char(character),
            CrosstermModifiers::CONTROL | CrosstermModifiers::ALT,
        )));
        assert!(!recognizer(&KeyEvent::new_with_kind(
            KeyCode::Char(character),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Repeat,
        )));
        assert!(!recognizer(&KeyEvent::new_with_kind(
            KeyCode::Char(character),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Release,
        )));
    }

    #[test]
    fn reserves_cli_shortcuts_for_all_event_kinds() {
        assert!(is_intercepted_shortcut(&KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Repeat,
        )));
        assert!(is_intercepted_shortcut(&KeyEvent::new_with_kind(
            KeyCode::Char('s'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Release,
        )));
        assert!(is_intercepted_shortcut(&KeyEvent::new_with_kind(
            KeyCode::Char('f'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Repeat,
        )));
        for character in ['c', 'x'] {
            assert!(is_intercepted_shortcut(&KeyEvent::new_with_kind(
                KeyCode::Char(character),
                CrosstermModifiers::CONTROL,
                KeyEventKind::Repeat,
            )));
            assert!(is_intercepted_shortcut(&KeyEvent::new_with_kind(
                KeyCode::Char(character),
                CrosstermModifiers::CONTROL,
                KeyEventKind::Release,
            )));
        }
        assert!(!is_intercepted_shortcut(&KeyEvent::new(
            KeyCode::Char('z'),
            CrosstermModifiers::CONTROL,
        )));
        assert!(!is_intercepted_shortcut(&KeyEvent::new(
            KeyCode::Char('c'),
            CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
        )));
    }
}

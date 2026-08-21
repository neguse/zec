/// Direction used when moving between open tabs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Previous,
    Next,
}

/// Returns the adjacent tab index, wrapping at either end.
///
/// An empty tab list or an invalid current index has no adjacent tab.
pub fn adjacent_index(current: usize, len: usize, direction: Direction) -> Option<usize> {
    if len == 0 || current >= len {
        return None;
    }

    match direction {
        Direction::Previous if current == 0 => Some(len - 1),
        Direction::Previous => Some(current - 1),
        Direction::Next if current + 1 == len => Some(0),
        Direction::Next => Some(current + 1),
    }
}

/// Display-only state for one tab.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabLabel {
    pub name: String,
    pub dirty: bool,
    pub conflict: bool,
}

/// Formats the tab portion of the editor status line.
///
/// A single tab is shown without navigation decoration. With multiple tabs,
/// every name and state marker remains visible and the active tab is bracketed.
/// Conflicts use `!`, dirty tabs use `+`, and conflicts take precedence when
/// both states are present.
/// An invalid active index is represented by `?` rather than attributing the
/// active state to the wrong tab.
pub fn format_status(labels: &[TabLabel], active: usize) -> String {
    let Some(first) = labels.first() else {
        return String::new();
    };

    if labels.len() == 1 {
        let mut status = first.name.clone();
        if first.conflict {
            status.push('!');
        } else if first.dirty {
            status.push('+');
        }
        return status;
    }

    let mut status = if active < labels.len() {
        format!("{}/{}", active + 1, labels.len())
    } else {
        format!("?/{}", labels.len())
    };

    for (index, label) in labels.iter().enumerate() {
        status.push(' ');
        let is_active = index == active;
        if is_active {
            status.push('[');
        }
        status.push_str(&label.name);
        if label.conflict {
            status.push('!');
        } else if label.dirty {
            status.push('+');
        }
        if is_active {
            status.push(']');
        }
    }

    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(name: &str, dirty: bool, conflict: bool) -> TabLabel {
        TabLabel {
            name: name.to_owned(),
            dirty,
            conflict,
        }
    }

    #[test]
    fn adjacent_index_wraps_in_both_directions() {
        assert_eq!(adjacent_index(0, 3, Direction::Previous), Some(2));
        assert_eq!(adjacent_index(1, 3, Direction::Previous), Some(0));
        assert_eq!(adjacent_index(1, 3, Direction::Next), Some(2));
        assert_eq!(adjacent_index(2, 3, Direction::Next), Some(0));
        assert_eq!(adjacent_index(0, 1, Direction::Previous), Some(0));
        assert_eq!(adjacent_index(0, 1, Direction::Next), Some(0));
    }

    #[test]
    fn adjacent_index_rejects_empty_and_out_of_range_state() {
        assert_eq!(adjacent_index(0, 0, Direction::Previous), None);
        assert_eq!(adjacent_index(0, 0, Direction::Next), None);
        assert_eq!(adjacent_index(3, 3, Direction::Previous), None);
        assert_eq!(adjacent_index(usize::MAX, 3, Direction::Next), None);
    }

    #[test]
    fn single_tab_uses_its_full_name_and_dirty_marker() {
        assert_eq!(
            format_status(&[label("src/nested/main.rs", false, false)], 0),
            "src/nested/main.rs"
        );
        assert_eq!(
            format_status(&[label("src/nested/main.rs", true, false)], 0),
            "src/nested/main.rs+"
        );
    }

    #[test]
    fn conflict_marker_takes_priority_over_dirty_marker() {
        assert_eq!(
            format_status(&[label("main.rs", false, true)], 0),
            "main.rs!"
        );
        assert_eq!(
            format_status(&[label("main.rs", true, true)], 0),
            "main.rs!"
        );
    }

    #[test]
    fn multiple_tabs_show_position_active_tab_and_every_state_marker() {
        let labels = [
            label("main.rs", false, false),
            label("README.md", true, false),
            label("notes.txt", true, true),
        ];

        assert_eq!(
            format_status(&labels, 1),
            "2/3 main.rs [README.md+] notes.txt!"
        );
    }

    #[test]
    fn status_preserves_unicode_and_empty_names() {
        let labels = [
            label("日本語.rs", true, false),
            label("", false, true),
            label("🦀.md", false, false),
        ];

        assert_eq!(format_status(&labels, 1), "2/3 日本語.rs+ [!] 🦀.md");
    }

    #[test]
    fn status_handles_empty_and_out_of_range_state() {
        assert_eq!(format_status(&[], 0), "");
        assert_eq!(format_status(&[], usize::MAX), "");

        let labels = [
            label("main.rs", false, false),
            label("README.md", true, true),
        ];
        assert_eq!(format_status(&labels, 2), "?/2 main.rs README.md!");
        assert_eq!(format_status(&labels, usize::MAX), "?/2 main.rs README.md!");
    }
}

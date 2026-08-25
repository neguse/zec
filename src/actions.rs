use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TerminalAction {
    CommandPalette,
    NewFile,
    OpenFile,
    QuickOpen,
    ProjectSearch,
    CloseTab,
    PreviousTab,
    NextTab,
    Find,
    Replace,
    GoToLine,
    Reload,
    Save,
    Quit,
    ShowCompletions,
    Hover,
    ProjectDiagnostics,
    GoToDefinition,
    GoToTypeDefinition,
    FindReferences,
    ProjectSymbols,
    NavigateBack,
    NavigateForward,
    RenameSymbol,
    CodeActions,
    FormatDocument,
    FormatSelection,
    Undo,
    Redo,
    Copy,
    Cut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionContext {
    pub has_repository: bool,
    pub has_file: bool,
    pub has_language_server: bool,
    pub can_navigate_back: bool,
    pub can_navigate_forward: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionDescriptor {
    pub action: TerminalAction,
    pub id: &'static str,
    pub name: &'static str,
    pub key_binding: &'static str,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionMatch {
    pub descriptor: ActionDescriptor,
    pub score: usize,
}

const ACTIONS: &[(TerminalAction, &str, &str, &str)] = &[
    (
        TerminalAction::CommandPalette,
        "command_palette::Toggle",
        "Command Palette",
        "F1",
    ),
    (
        TerminalAction::NewFile,
        "workspace::NewFile",
        "New File",
        "Ctrl-N",
    ),
    (
        TerminalAction::OpenFile,
        "workspace::Open",
        "Open File",
        "Ctrl-O",
    ),
    (
        TerminalAction::QuickOpen,
        "file_finder::Toggle",
        "Quick Open",
        "Ctrl-P",
    ),
    (
        TerminalAction::ProjectSearch,
        "project_search::ToggleFocus",
        "Project Search",
        "Alt-F",
    ),
    (
        TerminalAction::CloseTab,
        "pane::CloseActiveItem",
        "Close Tab",
        "Ctrl-W",
    ),
    (
        TerminalAction::PreviousTab,
        "pane::ActivatePreviousItem",
        "Previous Tab",
        "Ctrl-PgUp",
    ),
    (
        TerminalAction::NextTab,
        "pane::ActivateNextItem",
        "Next Tab",
        "Ctrl-PgDn",
    ),
    (
        TerminalAction::Find,
        "buffer_search::Deploy",
        "Find",
        "Ctrl-F",
    ),
    (
        TerminalAction::Replace,
        "buffer_search::DeployReplace",
        "Find and Replace",
        "Ctrl-H",
    ),
    (
        TerminalAction::GoToLine,
        "go_to_line::Toggle",
        "Go to Line/Column",
        "Ctrl-G",
    ),
    (
        TerminalAction::Reload,
        "workspace::ReloadActiveItem",
        "Reload from Disk",
        "Ctrl-R",
    ),
    (TerminalAction::Save, "workspace::Save", "Save", "Ctrl-S"),
    (
        TerminalAction::ShowCompletions,
        "editor::ShowCompletions",
        "Show Completions",
        "Ctrl-Space",
    ),
    (TerminalAction::Hover, "editor::Hover", "Show Hover", "F2"),
    (
        TerminalAction::ProjectDiagnostics,
        "diagnostics::Deploy",
        "Project Diagnostics",
        "F8",
    ),
    (
        TerminalAction::GoToDefinition,
        "editor::GoToDefinition",
        "Go to Definition",
        "F12",
    ),
    (
        TerminalAction::GoToTypeDefinition,
        "editor::GoToTypeDefinition",
        "Go to Type Definition",
        "Alt-F12",
    ),
    (
        TerminalAction::FindReferences,
        "editor::FindAllReferences",
        "Find All References",
        "Shift-F12",
    ),
    (
        TerminalAction::ProjectSymbols,
        "project_symbols::Toggle",
        "Project Symbols",
        "Ctrl-T",
    ),
    (
        TerminalAction::NavigateBack,
        "pane::GoBack",
        "Go Back",
        "Alt-Left",
    ),
    (
        TerminalAction::NavigateForward,
        "pane::GoForward",
        "Go Forward",
        "Alt-Right",
    ),
    (
        TerminalAction::RenameSymbol,
        "editor::Rename",
        "Rename Symbol",
        "F6",
    ),
    (
        TerminalAction::CodeActions,
        "editor::ToggleCodeActions",
        "Code Actions",
        "Ctrl-.",
    ),
    (
        TerminalAction::FormatDocument,
        "editor::Format",
        "Format Document",
        "Shift-Alt-F",
    ),
    (
        TerminalAction::FormatSelection,
        "editor::FormatSelections",
        "Format Selection",
        "Ctrl-Alt-F",
    ),
    (TerminalAction::Undo, "editor::Undo", "Undo", "Ctrl-Z"),
    (TerminalAction::Redo, "editor::Redo", "Redo", "Ctrl-Y"),
    (TerminalAction::Copy, "editor::Copy", "Copy", "Ctrl-C"),
    (TerminalAction::Cut, "editor::Cut", "Cut", "Ctrl-X"),
    (TerminalAction::Quit, "zed::Quit", "Quit", "Ctrl-Q"),
];

pub fn actions(context: ActionContext) -> Vec<ActionDescriptor> {
    ACTIONS
        .iter()
        .map(|(action, id, name, key_binding)| ActionDescriptor {
            action: *action,
            id,
            name,
            key_binding,
            enabled: match action {
                TerminalAction::QuickOpen | TerminalAction::ProjectSearch => context.has_repository,
                TerminalAction::Reload => context.has_file,
                TerminalAction::ShowCompletions
                | TerminalAction::Hover
                | TerminalAction::GoToDefinition
                | TerminalAction::GoToTypeDefinition
                | TerminalAction::FindReferences
                | TerminalAction::RenameSymbol
                | TerminalAction::CodeActions
                | TerminalAction::FormatDocument
                | TerminalAction::FormatSelection => context.has_language_server,
                TerminalAction::ProjectSymbols => {
                    context.has_repository && context.has_language_server
                }
                TerminalAction::NavigateBack => context.can_navigate_back,
                TerminalAction::NavigateForward => context.can_navigate_forward,
                TerminalAction::ProjectDiagnostics => true,
                _ => true,
            },
        })
        .collect()
}

pub fn search(context: ActionContext, query: &str) -> Vec<ActionMatch> {
    let query = query.trim().to_lowercase();
    let mut matches = actions(context)
        .into_iter()
        .filter_map(|descriptor| {
            let name = descriptor.name.to_lowercase();
            let id = descriptor.id.to_lowercase();
            let score = if query.is_empty() {
                3
            } else if name.starts_with(&query) {
                0
            } else if name.contains(&query) {
                1
            } else if id.contains(&query) {
                2
            } else {
                return None;
            };
            Some(ActionMatch { descriptor, score })
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.score
            .cmp(&right.score)
            .then_with(|| left.descriptor.name.cmp(right.descriptor.name))
            .then_with(|| left.descriptor.id.cmp(right.descriptor.id))
    });
    matches
}

impl TerminalAction {
    pub fn dispatch_action_id(self) -> &'static str {
        match self {
            Self::CommandPalette => "zec::CommandPalette",
            Self::NewFile => "zec::NewFile",
            Self::OpenFile => "zec::OpenFile",
            Self::QuickOpen => "zec::QuickOpen",
            Self::ProjectSearch => "zec::ProjectSearch",
            Self::CloseTab => "zec::CloseTab",
            Self::PreviousTab => "zec::PreviousTab",
            Self::NextTab => "zec::NextTab",
            Self::Find => "zec::Find",
            Self::Replace => "zec::Replace",
            Self::GoToLine => "zec::GoToLine",
            Self::Reload => "zec::Reload",
            Self::Save => "zec::Save",
            Self::Quit => "zec::Quit",
            Self::ShowCompletions => "zec::ShowCompletions",
            Self::Hover => "zec::Hover",
            Self::ProjectDiagnostics => "zec::ProjectDiagnostics",
            Self::GoToDefinition => "zec::GoToDefinition",
            Self::GoToTypeDefinition => "zec::GoToTypeDefinition",
            Self::FindReferences => "zec::FindReferences",
            Self::ProjectSymbols => "zec::ProjectSymbols",
            Self::NavigateBack => "zec::NavigateBack",
            Self::NavigateForward => "zec::NavigateForward",
            Self::RenameSymbol => "zec::RenameSymbol",
            Self::CodeActions => "zec::CodeActions",
            Self::FormatDocument => "zec::FormatDocument",
            Self::FormatSelection => "zec::FormatSelection",
            Self::Undo => "zec::Undo",
            Self::Redo => "zec::Redo",
            Self::Copy => "zec::Copy",
            Self::Cut => "zec::Cut",
        }
    }

    pub fn shortcut_event(self) -> KeyEvent {
        let (code, modifiers) = match self {
            Self::CommandPalette => (KeyCode::F(1), KeyModifiers::NONE),
            Self::NewFile => (KeyCode::Char('n'), KeyModifiers::CONTROL),
            Self::OpenFile => (KeyCode::Char('o'), KeyModifiers::CONTROL),
            Self::QuickOpen => (KeyCode::Char('p'), KeyModifiers::CONTROL),
            Self::ProjectSearch => (KeyCode::Char('f'), KeyModifiers::ALT),
            Self::CloseTab => (KeyCode::Char('w'), KeyModifiers::CONTROL),
            Self::PreviousTab => (KeyCode::PageUp, KeyModifiers::CONTROL),
            Self::NextTab => (KeyCode::PageDown, KeyModifiers::CONTROL),
            Self::Find => (KeyCode::Char('f'), KeyModifiers::CONTROL),
            Self::Replace => (KeyCode::Char('h'), KeyModifiers::CONTROL),
            Self::GoToLine => (KeyCode::Char('g'), KeyModifiers::CONTROL),
            Self::Reload => (KeyCode::Char('r'), KeyModifiers::CONTROL),
            Self::Save => (KeyCode::Char('s'), KeyModifiers::CONTROL),
            Self::Quit => (KeyCode::Char('q'), KeyModifiers::CONTROL),
            Self::ShowCompletions => (KeyCode::Char(' '), KeyModifiers::CONTROL),
            Self::Hover => (KeyCode::F(2), KeyModifiers::NONE),
            Self::ProjectDiagnostics => (KeyCode::F(8), KeyModifiers::NONE),
            Self::GoToDefinition => (KeyCode::F(12), KeyModifiers::NONE),
            Self::GoToTypeDefinition => (KeyCode::F(12), KeyModifiers::ALT),
            Self::FindReferences => (KeyCode::F(12), KeyModifiers::SHIFT),
            Self::ProjectSymbols => (KeyCode::Char('t'), KeyModifiers::CONTROL),
            Self::NavigateBack => (KeyCode::Left, KeyModifiers::ALT),
            Self::NavigateForward => (KeyCode::Right, KeyModifiers::ALT),
            Self::RenameSymbol => (KeyCode::F(6), KeyModifiers::NONE),
            Self::CodeActions => (KeyCode::Char('.'), KeyModifiers::CONTROL),
            Self::FormatDocument => (KeyCode::Char('f'), KeyModifiers::SHIFT | KeyModifiers::ALT),
            Self::FormatSelection => (
                KeyCode::Char('f'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::Undo => (KeyCode::Char('z'), KeyModifiers::CONTROL),
            Self::Redo => (KeyCode::Char('y'), KeyModifiers::CONTROL),
            Self::Copy => (KeyCode::Char('c'), KeyModifiers::CONTROL),
            Self::Cut => (KeyCode::Char('x'), KeyModifiers::CONTROL),
        };
        KeyEvent::new(code, modifiers)
    }
}

pub fn terminalize_keymap_action_ids(content: &str) -> String {
    ACTIONS
        .iter()
        .fold(content.to_owned(), |content, (action, id, _, _)| {
            content.replace(
                &format!("\"{id}\""),
                &format!("\"{}\"", action.dispatch_action_id()),
            )
        })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const FULL: ActionContext = ActionContext {
        has_repository: true,
        has_file: true,
        has_language_server: true,
        can_navigate_back: true,
        can_navigate_forward: true,
    };

    #[test]
    fn action_ids_are_unique_and_every_action_has_a_portable_key() {
        let actions = actions(FULL);
        assert_eq!(
            actions
                .iter()
                .map(|action| action.id)
                .collect::<HashSet<_>>()
                .len(),
            actions.len()
        );
        assert!(actions.iter().all(|action| !action.key_binding.is_empty()));
    }

    #[test]
    fn search_is_deterministic_and_exposes_context_state() {
        let context = ActionContext {
            has_repository: false,
            has_file: false,
            has_language_server: false,
            can_navigate_back: false,
            can_navigate_forward: false,
        };
        let first = search(context, "project");
        let second = search(context, "project");
        assert_eq!(first, second);
        assert_eq!(first[0].descriptor.name, "Project Diagnostics");
        assert!(first[0].descriptor.enabled);
        assert!(
            first
                .iter()
                .any(|item| item.descriptor.name == "Project Search" && !item.descriptor.enabled)
        );
    }

    #[test]
    fn semantic_actions_follow_language_server_requirements() {
        let disabled = actions(ActionContext {
            has_repository: true,
            has_file: true,
            has_language_server: false,
            can_navigate_back: false,
            can_navigate_forward: false,
        });
        let enabled = actions(FULL);
        for action in [
            TerminalAction::ShowCompletions,
            TerminalAction::Hover,
            TerminalAction::GoToDefinition,
            TerminalAction::GoToTypeDefinition,
            TerminalAction::FindReferences,
            TerminalAction::RenameSymbol,
            TerminalAction::CodeActions,
            TerminalAction::FormatDocument,
            TerminalAction::FormatSelection,
        ] {
            assert!(
                !disabled
                    .iter()
                    .find(|item| item.action == action)
                    .unwrap()
                    .enabled
            );
            assert!(
                enabled
                    .iter()
                    .find(|item| item.action == action)
                    .unwrap()
                    .enabled
            );
        }
        for context_actions in [&disabled, &enabled] {
            assert!(
                context_actions
                    .iter()
                    .find(|item| item.action == TerminalAction::ProjectDiagnostics)
                    .unwrap()
                    .enabled
            );
        }
    }

    #[test]
    fn keymap_aliases_preserve_public_ids_and_map_every_terminal_action_exactly() {
        let public = actions(FULL);
        let source = format!(
            "[{}]",
            public
                .iter()
                .map(|descriptor| format!("\"{}\"", descriptor.id))
                .collect::<Vec<_>>()
                .join(",")
        );
        let terminalized = terminalize_keymap_action_ids(&source);

        for descriptor in &public {
            assert!(
                !terminalized.contains(&format!("\"{}\"", descriptor.id)),
                "public action ID was not terminalized: {}",
                descriptor.id
            );
            assert!(
                terminalized.contains(&format!("\"{}\"", descriptor.action.dispatch_action_id())),
                "terminal action ID is absent for {}",
                descriptor.id
            );
        }
        assert_eq!(
            terminalize_keymap_action_ids(
                r#"{"bindings":{"f9":"prefix::editor::Hover","f10":"editor::HoverExtra"}}"#
            ),
            r#"{"bindings":{"f9":"prefix::editor::Hover","f10":"editor::HoverExtra"}}"#,
            "aliases must replace complete JSON string values only"
        );
    }
}

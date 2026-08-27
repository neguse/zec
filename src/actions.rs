use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TerminalAction {
    CommandPalette,
    ShowTerminalCapabilities,
    ToggleMarkdownPreview,
    ToggleAgentPanel,
    ToggleCollaborationPanel,
    InlineAssist,
    ShowEditPrediction,
    AcceptEditPrediction,
    AcceptNextWordEditPrediction,
    AcceptNextLineEditPrediction,
    ToggleEditPrediction,
    Extensions,
    SelectTheme,
    SelectIconTheme,
    OpenSettings,
    OpenKeymap,
    ReloadExtensions,
    CheckUpdates,
    NewFile,
    OpenFile,
    QuickOpen,
    ProjectSearch,
    ToggleProjectPanel,
    ToggleGitPanel,
    ToggleOutlinePanel,
    ToggleTerminalPanel,
    NewTerminal,
    RunTask,
    RerunTask,
    ToggleDebuggerPanel,
    StartDebugging,
    ToggleBreakpoint,
    ContinueDebugging,
    PauseDebugging,
    StopDebugging,
    StepOver,
    StepInto,
    StepOut,
    DebugRepl,
    CloseTab,
    PreviousTab,
    NextTab,
    SplitRight,
    SplitDown,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
    MoveItemLeft,
    MoveItemRight,
    MoveItemUp,
    MoveItemDown,
    GrowPane,
    ShrinkPane,
    PinTab,
    MoveTabLeft,
    MoveTabRight,
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
    ToggleFold,
    FoldAll,
    UnfoldAll,
    ToggleSoftWrap,
    ToggleInlayHints,
    AddSelectionAbove,
    AddSelectionBelow,
    SelectNextOccurrence,
    SelectAllOccurrences,
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
        TerminalAction::ShowTerminalCapabilities,
        "workspace::ShowTerminalCapabilities",
        "Show Terminal Capabilities",
        "F4",
    ),
    (
        TerminalAction::ToggleMarkdownPreview,
        "markdown_preview::OpenPreview",
        "Toggle Markdown Preview",
        "Ctrl-Shift-V",
    ),
    (
        TerminalAction::ToggleAgentPanel,
        "agent::ToggleFocus",
        "Toggle Agent Panel",
        "Ctrl-Shift-A",
    ),
    (
        TerminalAction::ToggleCollaborationPanel,
        "collab_panel::ToggleFocus",
        "Toggle Collaboration Panel",
        "Ctrl-Alt-C",
    ),
    (
        TerminalAction::InlineAssist,
        "assistant::InlineAssist",
        "Inline Assistant",
        "Ctrl-Enter",
    ),
    (
        TerminalAction::ShowEditPrediction,
        "editor::ShowEditPrediction",
        "Show Edit Prediction",
        "Alt-\\",
    ),
    (
        TerminalAction::AcceptEditPrediction,
        "editor::AcceptEditPrediction",
        "Accept Edit Prediction",
        "Alt-L",
    ),
    (
        TerminalAction::AcceptNextWordEditPrediction,
        "editor::AcceptNextWordEditPrediction",
        "Accept Next Prediction Word",
        "Alt-K",
    ),
    (
        TerminalAction::AcceptNextLineEditPrediction,
        "editor::AcceptNextLineEditPrediction",
        "Accept Next Prediction Line",
        "Alt-J",
    ),
    (
        TerminalAction::ToggleEditPrediction,
        "editor::ToggleEditPrediction",
        "Toggle Edit Predictions",
        "Ctrl-Alt-Shift-E",
    ),
    (
        TerminalAction::Extensions,
        "zed::Extensions",
        "Extensions",
        "Ctrl-Shift-X",
    ),
    (
        TerminalAction::SelectTheme,
        "theme_selector::Toggle",
        "Select Theme",
        "Ctrl-Alt-T",
    ),
    (
        TerminalAction::SelectIconTheme,
        "icon_theme_selector::Toggle",
        "Select Icon Theme",
        "Ctrl-Alt-I",
    ),
    (
        TerminalAction::OpenSettings,
        "zed::OpenSettingsFile",
        "Open Settings File",
        "Ctrl-,",
    ),
    (
        TerminalAction::OpenKeymap,
        "zed::OpenKeymapFile",
        "Open Keymap File",
        "Ctrl-Alt-,",
    ),
    (
        TerminalAction::ReloadExtensions,
        "zed::ReloadExtensions",
        "Reload Extensions",
        "Ctrl-Alt-R",
    ),
    (
        TerminalAction::CheckUpdates,
        "auto_update::Check",
        "Check for Updates",
        "Ctrl-Alt-U",
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
        TerminalAction::ToggleProjectPanel,
        "project_panel::ToggleFocus",
        "Toggle Project Panel",
        "F7",
    ),
    (
        TerminalAction::ToggleGitPanel,
        "git_panel::ToggleFocus",
        "Toggle Git Panel",
        "Ctrl-Shift-G",
    ),
    (
        TerminalAction::ToggleOutlinePanel,
        "outline_panel::ToggleFocus",
        "Toggle Outline Panel",
        "F9",
    ),
    (
        TerminalAction::ToggleTerminalPanel,
        "terminal_panel::ToggleFocus",
        "Toggle Terminal Panel",
        "Ctrl-`",
    ),
    (
        TerminalAction::NewTerminal,
        "workspace::NewTerminal",
        "New Terminal",
        "Ctrl-Shift-`",
    ),
    (
        TerminalAction::RunTask,
        "task::Spawn",
        "Run Task",
        "Ctrl-Shift-B",
    ),
    (
        TerminalAction::RerunTask,
        "task::Rerun",
        "Rerun Last Task",
        "Ctrl-Alt-B",
    ),
    (
        TerminalAction::ToggleDebuggerPanel,
        "debug_panel::ToggleFocus",
        "Toggle Debugger Panel",
        "Ctrl-Shift-D",
    ),
    (
        TerminalAction::StartDebugging,
        "debugger::Start",
        "Start Debugging",
        "F5",
    ),
    (
        TerminalAction::ToggleBreakpoint,
        "editor::ToggleBreakpoint",
        "Toggle Breakpoint",
        "Ctrl-F9",
    ),
    (
        TerminalAction::ContinueDebugging,
        "debugger::Continue",
        "Continue Debugging",
        "Ctrl-F5",
    ),
    (
        TerminalAction::PauseDebugging,
        "debugger::Pause",
        "Pause Debugging",
        "Ctrl-F6",
    ),
    (
        TerminalAction::StopDebugging,
        "debugger::Stop",
        "Stop Debugging",
        "Shift-F5",
    ),
    (
        TerminalAction::StepOver,
        "debugger::StepOver",
        "Debug Step Over",
        "Alt-F10",
    ),
    (
        TerminalAction::StepInto,
        "debugger::StepInto",
        "Debug Step Into",
        "Alt-F11",
    ),
    (
        TerminalAction::StepOut,
        "debugger::StepOut",
        "Debug Step Out",
        "Alt-Shift-F11",
    ),
    (
        TerminalAction::DebugRepl,
        "debugger::FocusConsole",
        "Evaluate in Debug Console",
        "Ctrl-Shift-R",
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
        TerminalAction::SplitRight,
        "pane::SplitRight",
        "Split Pane Right",
        "F10",
    ),
    (
        TerminalAction::SplitDown,
        "pane::SplitDown",
        "Split Pane Down",
        "Shift-F10",
    ),
    (
        TerminalAction::FocusPaneLeft,
        "pane::ActivateLeft",
        "Focus Pane Left",
        "Ctrl-Alt-Left",
    ),
    (
        TerminalAction::FocusPaneRight,
        "pane::ActivateRight",
        "Focus Pane Right",
        "Ctrl-Alt-Right",
    ),
    (
        TerminalAction::FocusPaneUp,
        "pane::ActivateUp",
        "Focus Pane Up",
        "Ctrl-Alt-Up",
    ),
    (
        TerminalAction::FocusPaneDown,
        "pane::ActivateDown",
        "Focus Pane Down",
        "Ctrl-Alt-Down",
    ),
    (
        TerminalAction::MoveItemLeft,
        "pane::MoveItemLeft",
        "Move Tab to Left Pane",
        "Ctrl-Alt-Shift-Left",
    ),
    (
        TerminalAction::MoveItemRight,
        "pane::MoveItemRight",
        "Move Tab to Right Pane",
        "Ctrl-Alt-Shift-Right",
    ),
    (
        TerminalAction::MoveItemUp,
        "pane::MoveItemUp",
        "Move Tab to Upper Pane",
        "Ctrl-Alt-Shift-Up",
    ),
    (
        TerminalAction::MoveItemDown,
        "pane::MoveItemDown",
        "Move Tab to Lower Pane",
        "Ctrl-Alt-Shift-Down",
    ),
    (
        TerminalAction::GrowPane,
        "pane::IncreaseSize",
        "Grow Active Pane",
        "Ctrl-Alt-=",
    ),
    (
        TerminalAction::ShrinkPane,
        "pane::DecreaseSize",
        "Shrink Active Pane",
        "Ctrl-Alt--",
    ),
    (
        TerminalAction::PinTab,
        "pane::PinActiveItem",
        "Pin Active Tab",
        "Alt-Enter",
    ),
    (
        TerminalAction::MoveTabLeft,
        "pane::MoveItemLeftInTabBar",
        "Move Tab Left",
        "Ctrl-Shift-PgUp",
    ),
    (
        TerminalAction::MoveTabRight,
        "pane::MoveItemRightInTabBar",
        "Move Tab Right",
        "Ctrl-Shift-PgDn",
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
    (
        TerminalAction::ToggleFold,
        "editor::ToggleFold",
        "Toggle Fold",
        "F11",
    ),
    (
        TerminalAction::FoldAll,
        "editor::FoldAll",
        "Fold All",
        "Ctrl-F11",
    ),
    (
        TerminalAction::UnfoldAll,
        "editor::UnfoldAll",
        "Unfold All",
        "Shift-F11",
    ),
    (
        TerminalAction::ToggleSoftWrap,
        "editor::ToggleSoftWrap",
        "Toggle Soft Wrap",
        "Alt-Z",
    ),
    (
        TerminalAction::ToggleInlayHints,
        "editor::ToggleInlayHints",
        "Toggle Inlay Hints",
        "Ctrl-:",
    ),
    (
        TerminalAction::AddSelectionAbove,
        "editor::AddSelectionAbove",
        "Add Cursor Above",
        "Shift-Alt-Up",
    ),
    (
        TerminalAction::AddSelectionBelow,
        "editor::AddSelectionBelow",
        "Add Cursor Below",
        "Shift-Alt-Down",
    ),
    (
        TerminalAction::SelectNextOccurrence,
        "editor::SelectNext",
        "Add Selection to Next Occurrence",
        "Ctrl-D",
    ),
    (
        TerminalAction::SelectAllOccurrences,
        "editor::SelectAllMatches",
        "Select All Occurrences",
        "Ctrl-Shift-L",
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
                TerminalAction::QuickOpen
                | TerminalAction::ProjectSearch
                | TerminalAction::ToggleProjectPanel
                | TerminalAction::ToggleGitPanel => context.has_repository,
                TerminalAction::Reload => context.has_file,
                TerminalAction::ToggleMarkdownPreview => context.has_file,
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
            Self::ShowTerminalCapabilities => "zec::ShowTerminalCapabilities",
            Self::ToggleMarkdownPreview => "zec::ToggleMarkdownPreview",
            Self::ToggleAgentPanel => "zec::ToggleAgentPanel",
            Self::ToggleCollaborationPanel => "zec::ToggleCollaborationPanel",
            Self::InlineAssist => "zec::InlineAssist",
            Self::ShowEditPrediction => "zec::ShowEditPrediction",
            Self::AcceptEditPrediction => "zec::AcceptEditPrediction",
            Self::AcceptNextWordEditPrediction => "zec::AcceptNextWordEditPrediction",
            Self::AcceptNextLineEditPrediction => "zec::AcceptNextLineEditPrediction",
            Self::ToggleEditPrediction => "zec::ToggleEditPrediction",
            Self::Extensions => "zec::Extensions",
            Self::SelectTheme => "zec::SelectTheme",
            Self::SelectIconTheme => "zec::SelectIconTheme",
            Self::OpenSettings => "zec::OpenSettings",
            Self::OpenKeymap => "zec::OpenKeymap",
            Self::ReloadExtensions => "zec::ReloadExtensions",
            Self::CheckUpdates => "zec::CheckUpdates",
            Self::NewFile => "zec::NewFile",
            Self::OpenFile => "zec::OpenFile",
            Self::QuickOpen => "zec::QuickOpen",
            Self::ProjectSearch => "zec::ProjectSearch",
            Self::ToggleProjectPanel => "zec::ToggleProjectPanel",
            Self::ToggleGitPanel => "zec::ToggleGitPanel",
            Self::ToggleOutlinePanel => "zec::ToggleOutlinePanel",
            Self::ToggleTerminalPanel => "zec::ToggleTerminalPanel",
            Self::NewTerminal => "zec::NewTerminal",
            Self::RunTask => "zec::RunTask",
            Self::RerunTask => "zec::RerunTask",
            Self::ToggleDebuggerPanel => "zec::ToggleDebuggerPanel",
            Self::StartDebugging => "zec::StartDebugging",
            Self::ToggleBreakpoint => "zec::ToggleBreakpoint",
            Self::ContinueDebugging => "zec::ContinueDebugging",
            Self::PauseDebugging => "zec::PauseDebugging",
            Self::StopDebugging => "zec::StopDebugging",
            Self::StepOver => "zec::StepOver",
            Self::StepInto => "zec::StepInto",
            Self::StepOut => "zec::StepOut",
            Self::DebugRepl => "zec::DebugRepl",
            Self::CloseTab => "zec::CloseTab",
            Self::PreviousTab => "zec::PreviousTab",
            Self::NextTab => "zec::NextTab",
            Self::SplitRight => "zec::SplitRight",
            Self::SplitDown => "zec::SplitDown",
            Self::FocusPaneLeft => "zec::FocusPaneLeft",
            Self::FocusPaneRight => "zec::FocusPaneRight",
            Self::FocusPaneUp => "zec::FocusPaneUp",
            Self::FocusPaneDown => "zec::FocusPaneDown",
            Self::MoveItemLeft => "zec::MoveItemLeft",
            Self::MoveItemRight => "zec::MoveItemRight",
            Self::MoveItemUp => "zec::MoveItemUp",
            Self::MoveItemDown => "zec::MoveItemDown",
            Self::GrowPane => "zec::GrowPane",
            Self::ShrinkPane => "zec::ShrinkPane",
            Self::PinTab => "zec::PinTab",
            Self::MoveTabLeft => "zec::MoveTabLeft",
            Self::MoveTabRight => "zec::MoveTabRight",
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
            Self::ToggleFold => "zec::ToggleFold",
            Self::FoldAll => "zec::FoldAll",
            Self::UnfoldAll => "zec::UnfoldAll",
            Self::ToggleSoftWrap => "zec::ToggleSoftWrap",
            Self::ToggleInlayHints => "zec::ToggleInlayHints",
            Self::AddSelectionAbove => "zec::AddSelectionAbove",
            Self::AddSelectionBelow => "zec::AddSelectionBelow",
            Self::SelectNextOccurrence => "zec::SelectNextOccurrence",
            Self::SelectAllOccurrences => "zec::SelectAllOccurrences",
            Self::Undo => "zec::Undo",
            Self::Redo => "zec::Redo",
            Self::Copy => "zec::Copy",
            Self::Cut => "zec::Cut",
        }
    }

    pub fn shortcut_event(self) -> KeyEvent {
        let (code, modifiers) = match self {
            Self::CommandPalette => (KeyCode::F(1), KeyModifiers::NONE),
            Self::ShowTerminalCapabilities => (KeyCode::F(4), KeyModifiers::NONE),
            Self::ToggleMarkdownPreview => (
                KeyCode::Char('v'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            Self::ToggleAgentPanel => (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            Self::ToggleCollaborationPanel => (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::InlineAssist => (KeyCode::Enter, KeyModifiers::CONTROL),
            Self::ShowEditPrediction => (KeyCode::Char('\\'), KeyModifiers::ALT),
            Self::AcceptEditPrediction => (KeyCode::Char('l'), KeyModifiers::ALT),
            Self::AcceptNextWordEditPrediction => (KeyCode::Char('k'), KeyModifiers::ALT),
            Self::AcceptNextLineEditPrediction => (KeyCode::Char('j'), KeyModifiers::ALT),
            Self::ToggleEditPrediction => (
                KeyCode::Char('e'),
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            ),
            Self::Extensions => (
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            Self::SelectTheme => (
                KeyCode::Char('t'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::SelectIconTheme => (
                KeyCode::Char('i'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::OpenSettings => (KeyCode::Char(','), KeyModifiers::CONTROL),
            Self::OpenKeymap => (
                KeyCode::Char(','),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::ReloadExtensions => (
                KeyCode::Char('r'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::CheckUpdates => (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::NewFile => (KeyCode::Char('n'), KeyModifiers::CONTROL),
            Self::OpenFile => (KeyCode::Char('o'), KeyModifiers::CONTROL),
            Self::QuickOpen => (KeyCode::Char('p'), KeyModifiers::CONTROL),
            Self::ProjectSearch => (KeyCode::Char('f'), KeyModifiers::ALT),
            Self::ToggleProjectPanel => (KeyCode::F(7), KeyModifiers::NONE),
            Self::ToggleGitPanel => (
                KeyCode::Char('g'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            Self::ToggleOutlinePanel => (KeyCode::F(9), KeyModifiers::NONE),
            Self::ToggleTerminalPanel => (KeyCode::Char('`'), KeyModifiers::CONTROL),
            Self::NewTerminal => (
                KeyCode::Char('`'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            Self::RunTask => (
                KeyCode::Char('b'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            Self::RerunTask => (
                KeyCode::Char('b'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::ToggleDebuggerPanel => (
                KeyCode::Char('d'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            Self::StartDebugging => (KeyCode::F(5), KeyModifiers::NONE),
            Self::ToggleBreakpoint => (KeyCode::F(9), KeyModifiers::CONTROL),
            Self::ContinueDebugging => (KeyCode::F(5), KeyModifiers::CONTROL),
            Self::PauseDebugging => (KeyCode::F(6), KeyModifiers::CONTROL),
            Self::StopDebugging => (KeyCode::F(5), KeyModifiers::SHIFT),
            Self::StepOver => (KeyCode::F(10), KeyModifiers::ALT),
            Self::StepInto => (KeyCode::F(11), KeyModifiers::ALT),
            Self::StepOut => (KeyCode::F(11), KeyModifiers::ALT | KeyModifiers::SHIFT),
            Self::DebugRepl => (
                KeyCode::Char('r'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            Self::CloseTab => (KeyCode::Char('w'), KeyModifiers::CONTROL),
            Self::PreviousTab => (KeyCode::PageUp, KeyModifiers::CONTROL),
            Self::NextTab => (KeyCode::PageDown, KeyModifiers::CONTROL),
            Self::SplitRight => (KeyCode::F(10), KeyModifiers::NONE),
            Self::SplitDown => (KeyCode::F(10), KeyModifiers::SHIFT),
            Self::FocusPaneLeft => (KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::ALT),
            Self::FocusPaneRight => (KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::ALT),
            Self::FocusPaneUp => (KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::ALT),
            Self::FocusPaneDown => (KeyCode::Down, KeyModifiers::CONTROL | KeyModifiers::ALT),
            Self::MoveItemLeft => (
                KeyCode::Left,
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            ),
            Self::MoveItemRight => (
                KeyCode::Right,
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            ),
            Self::MoveItemUp => (
                KeyCode::Up,
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            ),
            Self::MoveItemDown => (
                KeyCode::Down,
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            ),
            Self::GrowPane => (
                KeyCode::Char('='),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::ShrinkPane => (
                KeyCode::Char('-'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Self::PinTab => (KeyCode::Enter, KeyModifiers::ALT),
            Self::MoveTabLeft => (KeyCode::PageUp, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            Self::MoveTabRight => (
                KeyCode::PageDown,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
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
            Self::ToggleFold => (KeyCode::F(11), KeyModifiers::NONE),
            Self::FoldAll => (KeyCode::F(11), KeyModifiers::CONTROL),
            Self::UnfoldAll => (KeyCode::F(11), KeyModifiers::SHIFT),
            Self::ToggleSoftWrap => (KeyCode::Char('z'), KeyModifiers::ALT),
            Self::ToggleInlayHints => (KeyCode::Char(':'), KeyModifiers::CONTROL),
            Self::AddSelectionAbove => (KeyCode::Up, KeyModifiers::SHIFT | KeyModifiers::ALT),
            Self::AddSelectionBelow => (KeyCode::Down, KeyModifiers::SHIFT | KeyModifiers::ALT),
            Self::SelectNextOccurrence => (KeyCode::Char('d'), KeyModifiers::CONTROL),
            Self::SelectAllOccurrences => (
                KeyCode::Char('l'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
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
        // Parameterized Editor actions remain native Zed actions in user
        // keymaps so their JSON payload is preserved. zec's built-in keys use
        // the unit terminal wrappers for the documented default parameters.
        .filter(|(action, _, _, _)| {
            !matches!(
                action,
                TerminalAction::AddSelectionAbove
                    | TerminalAction::AddSelectionBelow
                    | TerminalAction::SelectNextOccurrence
            )
        })
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
    fn keymap_aliases_preserve_parameterized_zed_actions_and_map_unit_actions_exactly() {
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
            if matches!(
                descriptor.action,
                TerminalAction::AddSelectionAbove
                    | TerminalAction::AddSelectionBelow
                    | TerminalAction::SelectNextOccurrence
            ) {
                assert!(terminalized.contains(&format!("\"{}\"", descriptor.id)));
                continue;
            }
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

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers as CrosstermModifiers};
use gpui::{Keystroke, Modifiers as GpuiModifiers};

use crate::workspace_model::Direction as PaneDirection;

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

pub fn is_reload(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('r')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_find(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('f')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_replace(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('h')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_go_to_line(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('g')
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

pub fn is_new_tab(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('n')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_open(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('o')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_quick_open(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('p')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_project_search(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('f')
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_toggle_project_panel(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && ((event.code == KeyCode::F(7) && event.modifiers == CrosstermModifiers::NONE)
            || (matches!(event.code, KeyCode::Char('e' | 'E'))
                && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)))
}

pub fn is_toggle_git_panel(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('g' | 'G'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_toggle_outline_panel(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && ((event.code == KeyCode::F(9) && event.modifiers == CrosstermModifiers::NONE)
            || (matches!(event.code, KeyCode::Char('o' | 'O'))
                && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)))
}

pub fn is_toggle_terminal_panel(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && ((event.code == KeyCode::F(3) && event.modifiers == CrosstermModifiers::NONE)
            || (event.code == KeyCode::Char('`') && event.modifiers == CrosstermModifiers::CONTROL))
}

pub fn is_inline_assist(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Enter
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_show_edit_prediction(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('\\')
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_accept_edit_prediction(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && ((matches!(event.code, KeyCode::Char('l' | 'L'))
            && event.modifiers == CrosstermModifiers::ALT)
            || (event.code == KeyCode::Tab && event.modifiers == CrosstermModifiers::ALT))
}

pub fn is_accept_next_word_edit_prediction(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('k' | 'K'))
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_accept_next_line_edit_prediction(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('j' | 'J'))
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_toggle_edit_prediction(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('e' | 'E'))
        && event.modifiers
            == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT | CrosstermModifiers::SHIFT)
}

pub fn is_edit_prediction_shortcut(event: &KeyEvent) -> bool {
    is_show_edit_prediction(event)
        || is_accept_edit_prediction(event)
        || is_accept_next_word_edit_prediction(event)
        || is_accept_next_line_edit_prediction(event)
        || is_toggle_edit_prediction(event)
}

pub fn is_new_terminal(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('`' | '~'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_run_task(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('b' | 'B'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_rerun_task(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('b' | 'B'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_toggle_debugger_panel(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('d' | 'D'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_start_debugging(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(5)
        && event.modifiers == CrosstermModifiers::NONE
}

pub fn is_toggle_breakpoint(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(9)
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_continue_debugging(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(5)
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_pause_debugging(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(6)
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_stop_debugging(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(5)
        && event.modifiers == CrosstermModifiers::SHIFT
}

pub fn is_step_over(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(10)
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_step_into(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(11)
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_step_out(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(11)
        && event.modifiers == (CrosstermModifiers::ALT | CrosstermModifiers::SHIFT)
}

pub fn is_debug_repl(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('r' | 'R'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_command_palette(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && ((event.code == KeyCode::F(1) && event.modifiers == CrosstermModifiers::NONE)
            || (matches!(event.code, KeyCode::Char('p' | 'P'))
                && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)))
}

pub fn is_extensions(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('x' | 'X'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_select_theme(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('t' | 'T'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_select_icon_theme(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('i' | 'I'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_open_settings(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char(',')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_open_keymap(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char(',')
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_reload_extensions(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('r' | 'R'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_check_updates(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('u' | 'U'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_terminal_capabilities(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(4)
        && event.modifiers == CrosstermModifiers::NONE
}

pub fn is_toggle_markdown_preview(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('v' | 'V'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_toggle_agent_panel(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('a' | 'A'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_toggle_collaboration_panel(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('c' | 'C'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_completion(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && ((event.code == KeyCode::Char(' ') && event.modifiers == CrosstermModifiers::CONTROL)
            || (event.code == KeyCode::Char('/') && event.modifiers == CrosstermModifiers::ALT))
}

pub fn is_hover(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(2)
        && event.modifiers == CrosstermModifiers::NONE
}

pub fn is_project_diagnostics(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(8)
        && event.modifiers == CrosstermModifiers::NONE
}

pub fn is_go_to_definition(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(12)
        && event.modifiers == CrosstermModifiers::NONE
}

pub fn is_go_to_type_definition(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(12)
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_find_references(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(12)
        && event.modifiers == CrosstermModifiers::SHIFT
}

pub fn is_project_symbols(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('t')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_rename_symbol(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(6)
        && event.modifiers == CrosstermModifiers::NONE
}

pub fn is_code_actions(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('.')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_format_document(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('f' | 'F'))
        && event.modifiers == (CrosstermModifiers::ALT | CrosstermModifiers::SHIFT)
}

pub fn is_format_selection(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('f' | 'F'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_toggle_fold(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(11)
        && event.modifiers == CrosstermModifiers::NONE
}

pub fn is_fold_all(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(11)
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_unfold_all(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(11)
        && event.modifiers == CrosstermModifiers::SHIFT
}

pub fn is_toggle_soft_wrap(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('z' | 'Z'))
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_toggle_inlay_hints(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && ((event.code == KeyCode::Char(':') && event.modifiers == CrosstermModifiers::CONTROL)
            || (event.code == KeyCode::Char(';')
                && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)))
}

pub fn is_add_selection_above(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Up
        && event.modifiers == (CrosstermModifiers::SHIFT | CrosstermModifiers::ALT)
}

pub fn is_add_selection_below(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Down
        && event.modifiers == (CrosstermModifiers::SHIFT | CrosstermModifiers::ALT)
}

pub fn is_select_next_occurrence(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('d' | 'D'))
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_select_all_occurrences(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('l' | 'L'))
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_advanced_editor_shortcut(event: &KeyEvent) -> bool {
    is_toggle_fold(event)
        || is_fold_all(event)
        || is_unfold_all(event)
        || is_toggle_soft_wrap(event)
        || is_toggle_inlay_hints(event)
        || is_add_selection_above(event)
        || is_add_selection_below(event)
        || is_select_next_occurrence(event)
        || is_select_all_occurrences(event)
}

pub fn is_undo(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('z')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_redo(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && ((event.code == KeyCode::Char('y') && event.modifiers == CrosstermModifiers::CONTROL)
            || (matches!(event.code, KeyCode::Char('z' | 'Z'))
                && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)))
}

pub fn is_navigation_back(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Left
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_navigation_forward(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Right
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_close_tab(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('w')
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_previous_tab(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::PageUp
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_next_tab(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::PageDown
        && event.modifiers == CrosstermModifiers::CONTROL
}

pub fn is_split_right(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(10)
        && event.modifiers == CrosstermModifiers::NONE
}

pub fn is_split_down(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::F(10)
        && event.modifiers == CrosstermModifiers::SHIFT
}

pub fn focus_pane_direction(event: &KeyEvent) -> Option<PaneDirection> {
    if event.kind != KeyEventKind::Press
        || event.modifiers != (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
    {
        return None;
    }
    match event.code {
        KeyCode::Left => Some(PaneDirection::Left),
        KeyCode::Right => Some(PaneDirection::Right),
        KeyCode::Up => Some(PaneDirection::Up),
        KeyCode::Down => Some(PaneDirection::Down),
        _ => None,
    }
}

pub fn move_item_direction(event: &KeyEvent) -> Option<PaneDirection> {
    if event.kind != KeyEventKind::Press
        || event.modifiers
            != (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT | CrosstermModifiers::SHIFT)
    {
        return None;
    }
    match event.code {
        KeyCode::Left => Some(PaneDirection::Left),
        KeyCode::Right => Some(PaneDirection::Right),
        KeyCode::Up => Some(PaneDirection::Up),
        KeyCode::Down => Some(PaneDirection::Down),
        _ => None,
    }
}

pub fn is_grow_pane(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && matches!(event.code, KeyCode::Char('=' | '+'))
        && event
            .modifiers
            .contains(CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_shrink_pane(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Char('-')
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
}

pub fn is_pin_tab(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::Enter
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_move_tab_left(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::PageUp
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_move_tab_right(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::PageDown
        && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
}

pub fn is_workspace_layout_shortcut(event: &KeyEvent) -> bool {
    is_toggle_project_panel(event)
        || is_toggle_git_panel(event)
        || is_toggle_outline_panel(event)
        || is_toggle_terminal_panel(event)
        || is_new_terminal(event)
        || is_split_right(event)
        || is_split_down(event)
        || focus_pane_direction(event).is_some()
        || move_item_direction(event).is_some()
        || is_grow_pane(event)
        || is_shrink_pane(event)
        || is_pin_tab(event)
        || is_move_tab_left(event)
        || is_move_tab_right(event)
}

pub fn is_scroll_page_up(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::PageUp
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_scroll_page_down(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Press
        && event.code == KeyCode::PageDown
        && event.modifiers == CrosstermModifiers::ALT
}

pub fn is_intercepted_shortcut(event: &KeyEvent) -> bool {
    is_workspace_layout_shortcut(event)
        || is_run_task(event)
        || is_rerun_task(event)
        || is_toggle_debugger_panel(event)
        || is_start_debugging(event)
        || is_toggle_breakpoint(event)
        || is_continue_debugging(event)
        || is_pause_debugging(event)
        || is_stop_debugging(event)
        || is_step_over(event)
        || is_step_into(event)
        || is_step_out(event)
        || is_debug_repl(event)
        || is_advanced_editor_shortcut(event)
        || is_command_palette(event)
        || is_toggle_markdown_preview(event)
        || is_completion(event)
        || is_hover(event)
        || is_project_diagnostics(event)
        || is_go_to_definition(event)
        || is_go_to_type_definition(event)
        || is_find_references(event)
        || is_project_symbols(event)
        || is_rename_symbol(event)
        || is_code_actions(event)
        || is_format_document(event)
        || is_format_selection(event)
        || is_undo(event)
        || is_redo(event)
        || (event.code == KeyCode::F(6) && event.modifiers == CrosstermModifiers::NONE)
        || (event.code == KeyCode::Char('.') && event.modifiers == CrosstermModifiers::CONTROL)
        || (matches!(event.code, KeyCode::Char('f' | 'F'))
            && (event.modifiers == (CrosstermModifiers::ALT | CrosstermModifiers::SHIFT)
                || event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)))
        || (event.code == KeyCode::Char('z') && event.modifiers == CrosstermModifiers::CONTROL)
        || (matches!(event.code, KeyCode::Char('z' | 'Z'))
            && event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT))
        || (event.code == KeyCode::Char('y') && event.modifiers == CrosstermModifiers::CONTROL)
        || is_navigation_back(event)
        || is_navigation_forward(event)
        || (event.modifiers == CrosstermModifiers::CONTROL
            && matches!(
                event.code,
                KeyCode::Char(
                    'c' | 'f' | 'g' | 'h' | 'n' | 'o' | 'p' | 'q' | 'r' | 's' | 'w' | 'x'
                ) | KeyCode::PageUp
                    | KeyCode::PageDown
            ))
        || (event.modifiers == CrosstermModifiers::ALT
            && matches!(
                event.code,
                KeyCode::Char('f') | KeyCode::PageUp | KeyCode::PageDown
            ))
        || (event.code == KeyCode::F(10)
            && matches!(
                event.modifiers,
                CrosstermModifiers::NONE | CrosstermModifiers::SHIFT
            ))
        || (event.code == KeyCode::F(7) && event.modifiers == CrosstermModifiers::NONE)
        || (event.code == KeyCode::F(3) && event.modifiers == CrosstermModifiers::NONE)
        || (event
            .modifiers
            .contains(CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
            && matches!(
                event.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Char('=' | '+' | '-')
            ))
        || (matches!(event.code, KeyCode::Char('b' | 'B'))
            && matches!(
                event.modifiers,
                modifiers if modifiers
                    == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
                    || modifiers
                        == (CrosstermModifiers::CONTROL | CrosstermModifiers::ALT)
            ))
        || (event.modifiers == CrosstermModifiers::ALT && event.code == KeyCode::Enter)
        || (event.modifiers == (CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT)
            && matches!(
                event.code,
                KeyCode::Char('e' | 'E' | 'g' | 'G' | 'v' | 'V' | '`' | '~')
                    | KeyCode::PageUp
                    | KeyCode::PageDown
            ))
        || (event.code == KeyCode::Char('`') && event.modifiers == CrosstermModifiers::CONTROL)
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
    fn recognizes_only_plain_control_r_press_as_reload() {
        assert_plain_control_press_only(is_reload, 'r');
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
    fn recognizes_only_plain_control_g_press_as_go_to_line() {
        assert_plain_control_press_only(is_go_to_line, 'g');
    }

    #[test]
    fn recognizes_only_plain_control_h_press_as_replace() {
        assert_plain_control_press_only(is_replace, 'h');
    }

    #[test]
    fn recognizes_only_plain_control_c_press_as_copy() {
        assert_plain_control_press_only(is_copy, 'c');
    }

    #[test]
    fn recognizes_only_plain_control_x_press_as_cut() {
        assert_plain_control_press_only(is_cut, 'x');
    }

    #[test]
    fn recognizes_only_plain_control_n_press_as_new_tab() {
        assert_plain_control_press_only(is_new_tab, 'n');
    }

    #[test]
    fn recognizes_only_plain_control_o_press_as_open() {
        assert_plain_control_press_only(is_open, 'o');
    }

    #[test]
    fn recognizes_only_plain_control_p_press_as_quick_open() {
        assert_plain_control_press_only(is_quick_open, 'p');
    }

    #[test]
    fn recognizes_only_plain_alt_f_press_as_project_search() {
        assert_plain_alt_key_press_only(is_project_search, KeyCode::Char('f'));
    }

    #[test]
    fn recognizes_only_plain_control_w_press_as_close_tab() {
        assert_plain_control_press_only(is_close_tab, 'w');
    }

    #[test]
    fn recognizes_only_plain_control_page_up_press_as_previous_tab() {
        assert_plain_control_key_press_only(is_previous_tab, KeyCode::PageUp);
        assert!(!is_previous_tab(&KeyEvent::new(
            KeyCode::PageDown,
            CrosstermModifiers::CONTROL,
        )));
    }

    #[test]
    fn recognizes_only_plain_control_page_down_press_as_next_tab() {
        assert_plain_control_key_press_only(is_next_tab, KeyCode::PageDown);
        assert!(!is_next_tab(&KeyEvent::new(
            KeyCode::PageUp,
            CrosstermModifiers::CONTROL,
        )));
    }

    #[test]
    fn recognizes_only_plain_alt_page_up_press_as_page_scroll_up() {
        assert_plain_alt_key_press_only(is_scroll_page_up, KeyCode::PageUp);
        assert!(!is_scroll_page_up(&KeyEvent::new(
            KeyCode::PageDown,
            CrosstermModifiers::ALT,
        )));
    }

    #[test]
    fn recognizes_only_plain_alt_page_down_press_as_page_scroll_down() {
        assert_plain_alt_key_press_only(is_scroll_page_down, KeyCode::PageDown);
        assert!(!is_scroll_page_down(&KeyEvent::new(
            KeyCode::PageUp,
            CrosstermModifiers::ALT,
        )));
    }

    fn assert_plain_control_press_only(recognizer: fn(&KeyEvent) -> bool, character: char) {
        assert_plain_control_key_press_only(recognizer, KeyCode::Char(character));
    }

    fn assert_plain_control_key_press_only(recognizer: fn(&KeyEvent) -> bool, code: KeyCode) {
        assert!(recognizer(&KeyEvent::new(
            code.clone(),
            CrosstermModifiers::CONTROL,
        )));
        assert!(!recognizer(&KeyEvent::new(
            code.clone(),
            CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
        )));
        assert!(!recognizer(&KeyEvent::new(
            code.clone(),
            CrosstermModifiers::CONTROL | CrosstermModifiers::ALT,
        )));
        assert!(!recognizer(&KeyEvent::new(
            code.clone(),
            CrosstermModifiers::NONE,
        )));
        assert!(!recognizer(&KeyEvent::new_with_kind(
            code.clone(),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Repeat,
        )));
        assert!(!recognizer(&KeyEvent::new_with_kind(
            code,
            CrosstermModifiers::CONTROL,
            KeyEventKind::Release,
        )));
    }

    fn assert_plain_alt_key_press_only(recognizer: fn(&KeyEvent) -> bool, code: KeyCode) {
        assert!(recognizer(&KeyEvent::new(
            code.clone(),
            CrosstermModifiers::ALT,
        )));
        assert!(!recognizer(&KeyEvent::new(
            code.clone(),
            CrosstermModifiers::ALT | CrosstermModifiers::SHIFT,
        )));
        assert!(!recognizer(&KeyEvent::new(
            code.clone(),
            CrosstermModifiers::ALT | CrosstermModifiers::CONTROL,
        )));
        assert!(!recognizer(&KeyEvent::new(
            code.clone(),
            CrosstermModifiers::NONE,
        )));
        assert!(!recognizer(&KeyEvent::new_with_kind(
            code.clone(),
            CrosstermModifiers::ALT,
            KeyEventKind::Repeat,
        )));
        assert!(!recognizer(&KeyEvent::new_with_kind(
            code,
            CrosstermModifiers::ALT,
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
        for character in ['c', 'g', 'h', 'n', 'o', 'p', 'r', 'w', 'x'] {
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
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            assert!(is_intercepted_shortcut(&KeyEvent::new_with_kind(
                KeyCode::Char('f'),
                CrosstermModifiers::ALT,
                kind,
            )));
        }
        for code in [KeyCode::PageUp, KeyCode::PageDown] {
            assert!(is_intercepted_shortcut(&KeyEvent::new_with_kind(
                code,
                CrosstermModifiers::CONTROL,
                KeyEventKind::Repeat,
            )));
            assert!(is_intercepted_shortcut(&KeyEvent::new_with_kind(
                code,
                CrosstermModifiers::CONTROL,
                KeyEventKind::Release,
            )));
            assert!(is_intercepted_shortcut(&KeyEvent::new(
                code,
                CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
            )));
        }
        for code in [KeyCode::PageUp, KeyCode::PageDown] {
            for kind in [
                KeyEventKind::Press,
                KeyEventKind::Repeat,
                KeyEventKind::Release,
            ] {
                assert!(is_intercepted_shortcut(&KeyEvent::new_with_kind(
                    code,
                    CrosstermModifiers::ALT,
                    kind,
                )));
            }
            assert!(!is_intercepted_shortcut(&KeyEvent::new(
                code,
                CrosstermModifiers::NONE,
            )));
            assert!(!is_intercepted_shortcut(&KeyEvent::new(
                code,
                CrosstermModifiers::SHIFT,
            )));
            assert!(!is_intercepted_shortcut(&KeyEvent::new(
                code,
                CrosstermModifiers::ALT | CrosstermModifiers::SHIFT,
            )));
        }
        assert!(is_intercepted_shortcut(&KeyEvent::new(
            KeyCode::Char('z'),
            CrosstermModifiers::CONTROL,
        )));
        assert!(!is_intercepted_shortcut(&KeyEvent::new(
            KeyCode::Char('c'),
            CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
        )));
    }

    #[test]
    fn recognizes_command_palette_and_portable_language_action_fallbacks() {
        assert!(is_command_palette(&KeyEvent::new(
            KeyCode::Char('P'),
            CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
        )));
        assert!(is_command_palette(&KeyEvent::new(
            KeyCode::F(1),
            CrosstermModifiers::NONE,
        )));
        assert!(is_completion(&KeyEvent::new(
            KeyCode::Char(' '),
            CrosstermModifiers::CONTROL,
        )));
        assert!(is_completion(&KeyEvent::new(
            KeyCode::Char('/'),
            CrosstermModifiers::ALT,
        )));
        assert!(is_hover(&KeyEvent::new(
            KeyCode::F(2),
            CrosstermModifiers::NONE,
        )));
        assert!(is_project_diagnostics(&KeyEvent::new(
            KeyCode::F(8),
            CrosstermModifiers::NONE,
        )));
        assert!(is_go_to_definition(&KeyEvent::new(
            KeyCode::F(12),
            CrosstermModifiers::NONE,
        )));
        assert!(is_go_to_type_definition(&KeyEvent::new(
            KeyCode::F(12),
            CrosstermModifiers::ALT,
        )));
        assert!(is_find_references(&KeyEvent::new(
            KeyCode::F(12),
            CrosstermModifiers::SHIFT,
        )));
        assert!(is_project_symbols(&KeyEvent::new(
            KeyCode::Char('t'),
            CrosstermModifiers::CONTROL,
        )));
        assert!(is_rename_symbol(&KeyEvent::new(
            KeyCode::F(6),
            CrosstermModifiers::NONE,
        )));
        assert!(is_code_actions(&KeyEvent::new(
            KeyCode::Char('.'),
            CrosstermModifiers::CONTROL,
        )));
        assert!(is_format_document(&KeyEvent::new(
            KeyCode::Char('F'),
            CrosstermModifiers::SHIFT | CrosstermModifiers::ALT,
        )));
        assert!(is_format_selection(&KeyEvent::new(
            KeyCode::Char('f'),
            CrosstermModifiers::CONTROL | CrosstermModifiers::ALT,
        )));
        assert!(is_undo(&KeyEvent::new(
            KeyCode::Char('z'),
            CrosstermModifiers::CONTROL,
        )));
        assert!(is_redo(&KeyEvent::new(
            KeyCode::Char('y'),
            CrosstermModifiers::CONTROL,
        )));
        assert!(is_navigation_back(&KeyEvent::new(
            KeyCode::Left,
            CrosstermModifiers::ALT,
        )));
        assert!(is_navigation_forward(&KeyEvent::new(
            KeyCode::Right,
            CrosstermModifiers::ALT,
        )));
        assert!(!is_command_palette(&KeyEvent::new_with_kind(
            KeyCode::F(1),
            CrosstermModifiers::NONE,
            KeyEventKind::Release,
        )));
    }

    #[test]
    fn recognizes_advanced_editor_shortcuts_without_modifier_aliases() {
        for event in [
            KeyEvent::new(KeyCode::F(11), CrosstermModifiers::NONE),
            KeyEvent::new(KeyCode::F(11), CrosstermModifiers::CONTROL),
            KeyEvent::new(KeyCode::F(11), CrosstermModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('z'), CrosstermModifiers::ALT),
            KeyEvent::new(KeyCode::Char(':'), CrosstermModifiers::CONTROL),
            KeyEvent::new(
                KeyCode::Char(';'),
                CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
            ),
            KeyEvent::new(
                KeyCode::Up,
                CrosstermModifiers::SHIFT | CrosstermModifiers::ALT,
            ),
            KeyEvent::new(
                KeyCode::Down,
                CrosstermModifiers::SHIFT | CrosstermModifiers::ALT,
            ),
            KeyEvent::new(KeyCode::Char('d'), CrosstermModifiers::CONTROL),
            KeyEvent::new(
                KeyCode::Char('l'),
                CrosstermModifiers::CONTROL | CrosstermModifiers::SHIFT,
            ),
        ] {
            assert!(is_advanced_editor_shortcut(&event), "{event:?}");
            assert!(is_intercepted_shortcut(&event), "{event:?}");
        }
        assert!(!is_advanced_editor_shortcut(&KeyEvent::new(
            KeyCode::F(11),
            CrosstermModifiers::ALT,
        )));
        assert!(!is_advanced_editor_shortcut(&KeyEvent::new_with_kind(
            KeyCode::Char('d'),
            CrosstermModifiers::CONTROL,
            KeyEventKind::Release,
        )));
    }
}

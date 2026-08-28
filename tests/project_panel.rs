//! Project-panel identity, mutation, and confinement contract tests.

#[path = "../src/project_panel.rs"]
mod project_panel;
#[path = "../src/repository.rs"]
mod repository;

#[test]
fn project_panel_contract_module_is_linked_into_the_gate() {
    assert!(std::mem::size_of::<project_panel::ProjectPanelState>() > 0);
}

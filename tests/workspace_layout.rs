//! Workspace-model and cell-layout contract tests.
//!
//! These modules are shared verbatim with the actual `zec` binary. Keeping a
//! dedicated integration target makes the milestone gate fail if either file
//! stops being independently deterministic or starts depending on UI state.

#[path = "../src/workspace_model.rs"]
mod workspace_model;
#[path = "../src/workspace_render.rs"]
mod workspace_render;

#[test]
fn workspace_layout_contract_modules_are_linked_into_the_gate() {
    let workspace = workspace_model::WorkspaceModel::new(workspace_model::ItemId(1));
    let plan = workspace_render::render_plan(&workspace, ratatui::layout::Rect::new(0, 0, 120, 40));

    assert_eq!(workspace.item_ids().len(), 1);
    assert_eq!(plan.panes.len(), 1);
    assert!(plan.hidden_docks.is_empty());
    workspace.validate().expect("fresh workspace is valid");
}

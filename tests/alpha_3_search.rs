//! Alpha 3 Zed-query and all-or-nothing replacement contract tests.

#[path = "../src/project_search.rs"]
mod project_search;

#[test]
fn alpha_3_search_contract_module_is_linked_into_the_gate() {
    let options = project_search::ProjectSearchOptions {
        query: "fixture".to_owned(),
        ..project_search::ProjectSearchOptions::default()
    };
    options
        .validate()
        .expect("bounded Zed project query is valid");
}

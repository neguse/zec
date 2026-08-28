//! Immutable-generation session and recovery contract tests.

#[path = "../src/workspace_model.rs"]
mod workspace_model;
#[path = "../src/workspace_session.rs"]
mod workspace_session;

#[test]
fn workspace_sessions_contract_modules_are_linked_into_the_gate() {
    assert_eq!(workspace_session::SESSION_SCHEMA_VERSION, 1);
    assert!(workspace_session::MAX_SESSION_BYTES > 0);
    assert!(workspace_session::MAX_RECOVERY_BLOB_BYTES > workspace_session::MAX_SESSION_BYTES);
}

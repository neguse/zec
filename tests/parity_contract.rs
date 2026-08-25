use std::{collections::BTreeSet, fs, path::Path};

use serde::Deserialize;

const REQUIRED_CAPABILITIES: &[&str] = &[
    "CORE_EDITING",
    "FILE_LIFECYCLE",
    "PROJECT_DISCOVERY_BASIC_SEARCH",
    "TERMINAL_LIFECYCLE",
    "PROJECT_SERVICE_GRAPH",
    "COMMAND_PALETTE",
    "LANGUAGE_INTELLIGENCE",
    "LANGUAGE_MULTIBUFFER",
    "SETTINGS_KEYMAP",
    "WORKSPACE_LAYOUT",
    "PROJECT_PANEL_FILE_OPERATIONS",
    "SEARCH_REPLACE_COMPLETE",
    "ADVANCED_EDITOR_PRESENTATION",
    "NAVIGATION_OUTLINE",
    "GIT_WORKFLOW",
    "TERMINAL_TASK_TEST",
    "DEBUGGER_REPL_NOTEBOOK",
    "SESSION_CRASH_RECOVERY",
    "EXTENSION_ECOSYSTEM",
    "RICH_CONTENT",
    "PACKAGE_UPDATE_PLATFORM",
    "REMOTE_DEVELOPMENT",
    "AI_AGENT_ASSISTANCE",
    "COLLABORATIVE_EDITING",
    "COLLABORATION_MEDIA",
];

#[derive(Deserialize)]
struct Contract {
    schema_version: u64,
    zed_revision: String,
    status_values: Vec<String>,
    delivery_modes: Vec<String>,
    milestones: Vec<String>,
    capabilities: Vec<Capability>,
}

#[derive(Deserialize)]
struct Capability {
    id: String,
    milestone: String,
    delivery: String,
    status: String,
    evidence: String,
}

#[test]
fn parity_contract_is_complete_consistent_and_pinned() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let contract_path = root.join("docs/zed-parity-v1.json");
    let contract: Contract =
        serde_json::from_slice(&fs::read(&contract_path).expect("read docs/zed-parity-v1.json"))
            .expect("parse docs/zed-parity-v1.json");

    assert_eq!(contract.schema_version, 1);
    assert_eq!(contract.status_values, ["planned", "candidate", "verified"]);
    assert_eq!(
        contract.delivery_modes,
        ["faithful", "terminal-adapted", "external-bridge"]
    );
    assert_eq!(
        contract.milestones,
        [
            "alpha-1", "alpha-2", "alpha-3", "beta-1", "beta-2", "parity-1"
        ]
    );

    let expected = REQUIRED_CAPABILITIES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let actual = contract
        .capabilities
        .iter()
        .map(|capability| capability.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual, expected,
        "capability IDs changed without updating the contract test"
    );
    assert_eq!(
        contract.capabilities.len(),
        actual.len(),
        "capability IDs must be unique"
    );

    for capability in &contract.capabilities {
        assert!(
            contract.milestones.contains(&capability.milestone),
            "unknown milestone for {}",
            capability.id
        );
        assert!(
            contract.delivery_modes.contains(&capability.delivery),
            "unknown delivery mode for {}",
            capability.id
        );
        assert!(
            contract.status_values.contains(&capability.status),
            "unknown status for {}",
            capability.id
        );
        let evidence = root.join(&capability.evidence);
        assert!(
            evidence.is_file(),
            "evidence path for {} does not exist: {}",
            capability.id,
            evidence.display()
        );
    }

    let cargo_toml = fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    let mut zed_dependency_count = 0;
    for line in cargo_toml
        .lines()
        .filter(|line| line.contains("git = \"https://github.com/zed-industries/zed.git\""))
    {
        zed_dependency_count += 1;
        assert!(
            line.contains(&format!("rev = \"{}\"", contract.zed_revision)),
            "Zed dependency does not use parity contract revision: {line}"
        );
    }
    assert!(
        zed_dependency_count >= 10,
        "unexpectedly found only {zed_dependency_count} pinned Zed dependencies"
    );
}

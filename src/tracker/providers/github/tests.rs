use serde_json::json;

use super::issues::dependencies_of;

#[test]
fn relations_become_dependencies() {
    let answer = json!({
        "repository": { "issue": { "blockedBy": { "nodes": [
            {
                "number": 210,
                "title": "restore the github actions minutes",
                "state": "OPEN",
                "repository": { "nameWithOwner": "Satwa/supercal-v2" }
            },
            {
                "number": 133,
                "title": "verify beta localization",
                "state": "CLOSED",
                "repository": { "nameWithOwner": "Satwa/supercal-v2" }
            }
        ] } } }
    });

    let dependencies = dependencies_of(&answer).expect("parsed");

    assert_eq!(dependencies.len(), 2);
    assert_eq!(dependencies[0].identifier, "Satwa/supercal-v2#210");
    assert_eq!(dependencies[0].title, "restore the github actions minutes");
    assert!(dependencies[0].open);
    assert!(!dependencies[1].open, "a closed blocker is a satisfied one");
}

#[test]
fn no_blockers_is_no_dependencies() {
    let none = json!({ "repository": { "issue": { "blockedBy": { "nodes": [] } } } });
    assert_eq!(dependencies_of(&none).expect("parsed"), Vec::new());
}

#[test]
fn an_issue_it_cannot_read_blocks_nothing() {
    // GitHub answers an unreadable repository with a null issue rather than an error,
    // and there is nothing to wait on in "I cannot see it".
    let null = json!({ "repository": null });
    assert_eq!(dependencies_of(&null).expect("parsed"), Vec::new());
}
